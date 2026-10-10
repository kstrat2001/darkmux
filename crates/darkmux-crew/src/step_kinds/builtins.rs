//! Built-in step kinds (#1230 Packet 2): `dispatch.internal`,
//! `dispatch.single_shot`, `procedural.shell`, `procedural.noop`.
//!
//! Each kind reads its parameters from `Step.config` through ONE typed
//! struct (`crate::step_config`), the same struct the mission-config gate
//! checks the document against. A missing required key or a key of the
//! wrong type is a loud `Err`, never a silent default that would mask an
//! operator/caller typo.
//!
//! **This is Tier 1 (#1352).** Every kind below is generic AND
//! config-driven — no per-mission control flow, only values read from
//! `Step.config`. This is the DEFAULT: before writing a new `StepKind`
//! anywhere (this crate's `step_kinds::patterns`, or bespoke inside a
//! mission's own module), check whether the actual need is just new
//! CONFIG on one of these four kinds. See `step_kinds::patterns`'s module
//! doc for the full three-tier picture.

use super::types::{
    CwdPolicy, MapDispatchOverride, OverrideDispatchCall, Port, SeatClaim, StepKind, StepOutcome, StepRunCtx,
};
use crate::dispatch_budget::DispatchBudget;
use crate::step_output::labels;
use crate::step_config::{
    load, load_checked, ConfigKind, DispatchInternalConfig, MapConfig, ModelCallConfig, NoopConfig,
    MapSource, ShellConfig, SingleShotConfig,
};
use crate::types::{Step, Task};
use darkmux_flow::payload::{DispatchEndPayload, DispatchStartPayload, ResultClass, StepResultPayload};
use darkmux_trajectory::FailedExec;
use darkmux_types::execution_id::ExecutionId;
use darkmux_types::session_id::{SessionId, SessionScope};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Compose a step kind's base prompt/message with the gathered output of
/// its already-`Complete` dependencies. Shared by `dispatch.internal` and
/// `dispatch.single_shot` so the "prior step outputs" framing is
/// identical across both. `input` iterates in `BTreeMap` key order
/// (dependency step id), so the composed text is deterministic regardless
/// of completion order.
fn compose_message(base: &str, input: &BTreeMap<String, String>) -> String {
    if input.is_empty() {
        return base.to_string();
    }
    let mut composed = String::new();
    for (dep_id, output) in input {
        composed.push_str(&format!("--- output of step `{dep_id}` ---\n{output}\n\n"));
    }
    composed.push_str(base);
    composed
}

/// (#2570) The identifier a LOCAL (non-`endpoint`) `dispatch.single_shot` /
/// `dispatch.map` step addresses — the SAME derivation each kind's `seat()`
/// already uses to decide what residency LOADS under (an explicit
/// `config.identifier` override, else the darkmux-namespaced form of
/// `model`). Before #2570 `seat()` computed this identifier to claim
/// residency and `run`/`run_map` then put the bare `config.model` string on
/// the wire, unchanged — a step naming a bare model with no override
/// LOADED `darkmux:<key>` and ADDRESSED `<key>`, the same load/wire split
/// #2240 closed for the main dispatch model and #2536 closed for the
/// compactor. This function is now the ONE place both kinds' `seat()` AND
/// both kinds' `run`/`run_map` compute it, so all four sites agree by
/// construction instead of by four independent copies of the same two-line
/// expression staying in sync by hand — the drift #2537 tracks. Collapsing
/// this with `managed_wire_model` / `compactor_wire_model_id` in
/// `dispatch_internal.rs` (which resolve against a `Profile`, not a `Step`)
/// is left to #2537; the shapes differ enough (a `Step.config` string vs. a
/// `Profile.models[]` lookup) that forcing one signature over both would
/// widen this fix rather than just closing it.
///
/// HOSTED steps (`config.endpoint` present) never call this: an endpoint
/// deployment name is not something darkmux loads into local residency, so
/// the bare `config.model` string is already the correct wire value there
/// — see each `seat()`'s own `UnmanagedEndpoint` short-circuit.
fn local_dispatch_wire_model_id(call: &ModelCallConfig) -> String {
    call.identifier.clone().unwrap_or_else(|| darkmux_gestalt::namespaced_identifier(&call.model, None))
}

/// The identifier a single-shot or map step addresses: the bare `model` for
/// a hosted step (an endpoint's deployment name, never loaded locally),
/// else the local identifier [`local_dispatch_wire_model_id`] derives.
fn step_wire_model(call: &ModelCallConfig, is_hosted: bool) -> String {
    if is_hosted {
        call.model.clone()
    } else {
        local_dispatch_wire_model_id(call)
    }
}

/// (Also fix, #2570/#2240 class) A LOCAL step now addresses a darkmux-
/// namespaced IDENTIFIER (`local_dispatch_wire_model_id`, above) rather
/// than a bare model key. LMStudio 400s on an identifier it has no
/// instance for instead of silently JIT-loading a fresh copy the way a
/// bare key would — a real behavior change from "loads badly at the wrong
/// context" to "fails outright" when the resident instance is gone between
/// this step's seat claim and its actual dispatch (a TTL expiry, an `lms
/// unload` / `darkmux machine eject` from another shell, an LMStudio
/// restart). `dispatch_internal::residency_lost_detail` already carries
/// this explanation for the main dispatch model's own local error path
/// (#2240); this is the same hint, attached at both of THIS module's local
/// arms (`dispatch.single_shot`'s local branch and `dispatch.map`'s
/// per-item local branch) so a step-kind dispatch failure reads the same
/// way a top-level dispatch failure already does — never silently, and
/// never with a plain "not found" that leaves the operator to rediscover
/// what #1274's namespace convention already explains.
fn with_residency_lost_hint(wire_model: &str, e: anyhow::Error) -> anyhow::Error {
    let detail = format!("{e:#}");
    match crate::dispatch_internal::residency_lost_detail(wire_model, &detail) {
        Some(msg) => e.context(msg),
        None => e,
    }
}

/// (#1230 Packet 3, reshaped by #2394) Best-effort role→profile→model
/// resolution for [`crate::step_kinds::StepKind::seat`] implementations —
/// NOT the dispatch's own strict preflight (that still runs in full,
/// separately, inside `dispatch::dispatch`/`dispatch_internal::dispatch`
/// when the step actually executes). This is purely a scheduling
/// classification: resolve `role_id` against the named (or default) profile
/// via the same `select_model` scoring every dispatch preflight uses, and
/// report what the winning seat actually IS.
///
/// Returns a [`SeatClaim`], never an `Option`. That is the whole #2394
/// change at this layer: the three genuinely different outcomes below used
/// to collapse into one `None`, which the scheduler then read as "remote".
///
/// - a LOCAL model resolved → [`SeatClaim::LocalModel`], wave-planned and
///   #1487 lease-protected;
/// - the winning model is on an UNMANAGED endpoint →
///   [`SeatClaim::UnmanagedEndpoint`], which is correct and SILENT: it was
///   never going to touch local residency;
/// - resolution BROKE (unresolvable role, unloadable registry, no active
///   profile, a local model missing `n_ctx`) →
///   [`SeatClaim::LocalModelUnresolved`], which the scheduler surfaces
///   loudly.
///
/// **Why that last one has to be named (#1509 review finding).** A seat that
/// does not reach gestalt gets no `ensure_wave_loaded` call — no wave load,
/// and (load-bearing) **no #1487 residency lease written** — so the dispatch
/// runs completely UNPROTECTED against a concurrent command's `Exclusive`
/// reconcile, which can evict its model mid-generation with zero warning.
/// Correct for a genuinely remote model; a real safety gap for every other
/// cause. Before #2394 the distinction lived in a private
/// `resolve_local_placement_or_warn` that `eprintln!`d from down here, where
/// it knew a seat label but not the step, the run, or the flow stream. Now
/// the CLAIM carries the reason up to the scheduler, which owns all three —
/// see `scheduler::run_step_graph`'s classification block.
pub fn resolve_local_seat(
    role_id: &str,
    profile_name: Option<&str>,
    config_path: Option<&str>,
    seat: &str,
) -> SeatClaim {
    match resolve_local_placement_inner(role_id, profile_name, config_path, seat) {
        Ok(placement) => SeatClaim::LocalModel(placement),
        Err(PlacementMiss::Unmanaged(slot)) => SeatClaim::UnmanagedEndpoint(slot),
        Err(PlacementMiss::ResolutionFailed(reason)) => SeatClaim::LocalModelUnresolved { reason },
    }
}

/// Why [`resolve_local_placement_inner`] returned no `Placement` — see
/// [`resolve_local_seat`]'s doc. `Remote` is the legitimate, silent case (an
/// endpoint-bearing model was never going to need local residency);
/// `ResolutionFailed` is the loud one.
enum PlacementMiss {
    Unmanaged(crate::step_kinds::EndpointSlot),
    ResolutionFailed(String),
}

/// [`resolve_local_seat`]'s body, classified per [`PlacementMiss`] instead
/// of collapsing every miss to a bare `None`. Pure logic, no `eprintln!`
/// here — the caller owns deciding which miss is worth surfacing, and
/// (#2394) the caller that can say it usefully is the scheduler.
fn resolve_local_placement_inner(
    role_id: &str,
    profile_name: Option<&str>,
    config_path: Option<&str>,
    seat: &str,
) -> std::result::Result<darkmux_gestalt::Placement, PlacementMiss> {
    // (#2329 review) The dispatch resolves an unnamed profile through the
    // machine-local `role_profiles` map FIRST (`target::
    // resolve_role_aware_profile_with`, #1547) and `default_profile` only as the
    // fallback. This placement used to read `default_profile` alone, so a
    // bound role (`role_profiles.coder = coder-qwen38` on a registry whose
    // default is `balanced`) had its wave load and lease model A while every
    // dispatch loaded model B — lived on 2026-09-04: a five-coder wave leased
    // turboquant and each coder then loaded qwen3.8 itself. Same map, same
    // precedence, read live like the dispatch does (test builds see an empty
    // map by construction, #811 — the pure core below takes it as a value).
    let mapped = crate::target::role_profile_binding(Some(role_id), profile_name);
    resolve_local_placement_inner_with(role_id, profile_name, mapped, config_path, seat)
}

/// Pure core of [`resolve_local_placement_inner`]: the `role_profiles`
/// binding arrives as a value so a test can drive the mapped arm.
fn resolve_local_placement_inner_with(
    role_id: &str,
    profile_name: Option<&str>,
    mapped: Option<String>,
    config_path: Option<&str>,
    seat: &str,
) -> std::result::Result<darkmux_gestalt::Placement, PlacementMiss> {
    use PlacementMiss::ResolutionFailed;

    let loaded = darkmux_profiles::profiles::load_registry(config_path)
        .map_err(|e| ResolutionFailed(format!("profile registry: {e}")))?;
    let roles = crate::loader::load_roles().map_err(|e| ResolutionFailed(format!("loading roles: {e}")))?;
    let role = roles
        .iter()
        .find(|r| r.id == role_id)
        .ok_or_else(|| ResolutionFailed(format!("role `{role_id}` not found")))?;
    // (#2902 step 3) The ONE resolver the dispatch itself routes on: the
    // same profile precedence (#2329: `role_profiles` before
    // `default_profile`, a dangling binding loud), the same `select_model`
    // with the utility set-aside (#2914: a step never runs on the machine's
    // utility model), and the same endpoint classification.
    let target = crate::target::resolve_in(&loaded.registry, role, profile_name, mapped, false)
        .and_then(|r| r.require(role_id, profile_name, &loaded.path))
        .map_err(|e| ResolutionFailed(format!("{e:#}")))?;
    // (#2902 review C4) Exhaustive on the kind, so a new kind is a compile
    // error at the placement decision.
    match target.kind {
        darkmux_types::EndpointKind::Managed(_) => {}
        darkmux_types::EndpointKind::Unmanaged => {
            return Err(PlacementMiss::Unmanaged(crate::step_kinds::EndpointSlot::of(&target.endpoint)))
        }
    }
    let pm = &target.model;
    // (#2902 step 4) The one n_ctx rule (`ProfileModel::require_n_ctx`).
    let min_ctx = pm.require_n_ctx().map_err(|e| ResolutionFailed(format!("{e:#}")))?;
    let identifier = darkmux_gestalt::namespaced_identifier(&pm.id, pm.identifier.as_deref());
    Ok(darkmux_gestalt::Placement {
        model_key: pm.id.clone(),
        identifier,
        min_ctx,
        seat: seat.to_string(),
    })
}

/// (#2902 step 3) A model step's `config.endpoint` through the one resolver
/// (`target::step_unmanaged_endpoint`): `Some` only for an UNMANAGED
/// endpoint, the hosted arm. An `endpoints` id resolves against the registry
/// the step names (`config.config_path`), else the default one.
fn step_endpoint(call: &ModelCallConfig) -> Result<Option<darkmux_types::ModelEndpoint>> {
    crate::target::step_unmanaged_endpoint(call.endpoint.as_ref(), call.config_path.as_deref())
}

/// (#3035) A step's `config.endpoint` when it names a MANAGED endpoint
/// (`target::step_managed_endpoint`): the local arm still carries that
/// endpoint's limits.
fn step_managed_endpoint(call: &ModelCallConfig) -> Result<Option<darkmux_types::ModelEndpoint>> {
    crate::target::step_managed_endpoint(call.endpoint.as_ref(), call.config_path.as_deref())
}

/// The hosted arm of a `dispatch.single_shot` call: both budgets before the
/// network, the call, its spend settled, and its `step.result` record pushed
/// onto `flow_records`.
fn hosted_single_shot_reply(
    step: &Step,
    req: &crate::single_shot::HostedSingleShotRequest<'_>,
    endpoint_label: Option<&str>,
    session: &SessionId,
    budget_caller: &crate::budget::BudgetCaller<'_>,
    ctx: Option<&StepRunCtx>,
    flow_records: &mut Vec<darkmux_flow::FlowRecord>,
) -> Result<crate::single_shot::SingleShotReply> {
    // (#2902 step 5) Both budgets before the network, never after:
    // the endpoint's window, then this call's dispatch cap. A breach
    // warns; an endpoint `wait` holds the call (the step stays live,
    // its heartbeat beating) until there is room. Nothing is clamped.
    crate::budget::admit_endpoint(req.endpoint, budget_caller)?;
    let dispatch_bucket =
        std::sync::Mutex::new(DispatchBudget::for_endpoint(req.endpoint).map_err(|e| anyhow::anyhow!(e))?);
    let budget = dispatch_bucket.lock().unwrap_or_else(|p| p.into_inner()).budget();

    let reply = match crate::single_shot::single_shot_chat_hosted(req) {
        Ok(reply) => reply,
        Err(e) => {
            charge_failed_hosted_step_call(
                &e,
                req,
                &dispatch_bucket,
                budget_caller,
                ctx,
                (&step.id, endpoint_label.unwrap_or_default()),
            );
            return Err(e).with_context(|| format!("step `{}` dispatch.single_shot (hosted)", step.id));
        }
    };
    crate::budget::settle_dispatch_live(
        &dispatch_bucket,
        crate::budget::conservative_hosted_spend(reply.counts.total_tokens(), req.max_tokens, &req.body()?),
        &step.id,
        budget_caller,
    );

    // (#1412) Surface actual spend the same way `dispatch_unmanaged`
    // embeds totals in its `dispatch complete` record, so a hosted
    // single-shot step's token usage is visible even without the
    // full per-step bucket regime.
    flow_records.push(darkmux_flow::FlowRecord {
        source: Some(darkmux_flow::FlowSource::Scheduler),
        model: Some(req.model.to_string()),
        ..darkmux_flow::FlowRecord::for_session_with(
            session,
            darkmux_flow::Level::Info,
            darkmux_flow::Category::Work,
            darkmux_flow::Stage::Dispatch,
            darkmux_flow::Payload::StepResult(hosted_single_shot_step_payload(
                &step.id,
                budget,
                req.max_tokens,
                req.max_tokens,
                &reply,
            )),
            step.id.clone(),
        )
    });
    Ok(reply)
}

/// The LOCAL arm of `dispatch.single_shot` (#3035): a local step whose
/// `config.endpoint` names a MANAGED endpoint answers to that endpoint's
/// limits like any other: its window gate, then this call's dispatch cap.
#[allow(clippy::too_many_arguments)]
fn local_single_shot_reply(
    step: &Step,
    managed_endpoint: Option<&darkmux_types::ModelEndpoint>,
    call: &ModelCallConfig,
    wire_model: &str,
    system: &str,
    user: &str,
    (max_tokens, timeout_seconds): (u32, u32),
    caller: &crate::budget::BudgetCaller<'_>,
) -> Result<crate::single_shot::SingleShotReply> {
    use crate::single_shot::{single_shot_chat, SingleShotRequest};
    let local_cap = match managed_endpoint {
        Some(ep) => {
            crate::budget::admit_endpoint(ep, caller)?;
            let bucket = Mutex::new(DispatchBudget::for_endpoint(ep).map_err(|e| anyhow::anyhow!(e))?);
            Some(bucket)
        }
        None => None,
    };
    let req = SingleShotRequest {
        base_url: None,
        model: wire_model,
        system,
        user,
        temperature: call.temperature(),
        max_tokens,
        timeout_seconds,
    };
    let reply = single_shot_chat(&req)
        .map_err(|e| with_residency_lost_hint(wire_model, e))
        .with_context(|| format!("step `{}` dispatch.single_shot (local)", step.id))?;
    if let Some(bucket) = &local_cap {
        crate::budget::settle_dispatch_live(
            bucket,
            crate::budget::conservative_spend(reply.counts.total_tokens(), max_tokens, &format!("{system}{user}")),
            &step.id,
            caller,
        );
    }
    Ok(reply)
}

/// (#3035) The limits a LOCAL step item answers to when its `config.endpoint`
/// names a managed endpoint: the endpoint (its window gate), the item's own
/// dispatch cap bucket, and who the budget records are about.
struct LocalLimits<'a> {
    endpoint: &'a darkmux_types::ModelEndpoint,
    bucket: &'a Mutex<DispatchBudget>,
    label: &'a str,
    caller: &'a crate::budget::BudgetCaller<'a>,
}

impl<'a> LocalLimits<'a> {
    /// The limits for one item, when its step names a managed endpoint.
    fn of(
        endpoint: Option<&'a darkmux_types::ModelEndpoint>,
        bucket: Option<&'a Mutex<DispatchBudget>>,
        label: &'a str,
        caller: &'a crate::budget::BudgetCaller<'a>,
    ) -> Option<Self> {
        endpoint.zip(bucket).map(|(endpoint, bucket)| Self { endpoint, bucket, label, caller })
    }
}

/// One item's own dispatch cap bucket, when its step names a managed endpoint.
fn item_cap_bucket(endpoint: Option<&darkmux_types::ModelEndpoint>) -> Result<Option<Mutex<DispatchBudget>>> {
    endpoint
        .map(|ep| DispatchBudget::for_endpoint(ep).map(Mutex::new).map_err(|e| anyhow::anyhow!(e)))
        .transpose()
}

/// Best-effort parse of `failed_tool_invocations` from the internal
/// runtime's `--json` envelope (a dispatch's stdout). In `--json` mode the
/// runtime prints a single-line JSON envelope to stdout (status goes to
/// stderr), so the whole buffer is the envelope; the last-non-empty-line
/// fallback is pure defense against an unexpected leading line. Returns
/// EMPTY on any parse miss or absent field — a soft signal must never fire
/// a FALSE alarm, so "couldn't tell" reads as "nothing failed."
pub fn parse_failed_verifiers(envelope_stdout: &str) -> Vec<FailedExec> {
    let as_json = |s: &str| serde_json::from_str::<serde_json::Value>(s.trim()).ok();
    let Some(v) = as_json(envelope_stdout).or_else(|| {
        envelope_stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .and_then(as_json)
    }) else {
        return Vec::new();
    };
    v.get("failed_tool_invocations")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| serde_json::from_value::<FailedExec>(e.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Wraps `dispatch::dispatch(DispatchOpts)` — a full agentic dispatch
/// through darkmux's internal Docker-bounded runtime.
///
/// **Assignment sourcing (#1230/#1341): the owning Task is the assignable
/// unit.** `role_id`/`profile_name`/`workdir`/`image` come from the Task
/// FIRST (`task.role_id` etc — see `types::Task`'s doc: like a Jira ticket
/// assigned to one crew member, these are properties of the whole job,
/// fixed for its duration) and fall back to the matching `Step.config` key
/// only when the Task leaves that field unset — so a Task built without
/// resource fields (an older caller, or a test) still works exactly as
/// before. `role_id` is REQUIRED from one source or the other. Remaining
/// `Step.config` keys (unaffected by this Task-sourcing — these are
/// per-dispatch mechanics, not job-level assignment): `message` (string,
/// the base prompt — prior-dependency output is prepended per
/// `compose_message`), `timeout_seconds` (u32, default 3600),
/// `config_path` (string, `--profiles-file` passthrough), `phase_id`
/// (string, threads a Phase-scoped context file into the dispatch — see
/// `DispatchOpts::phase_id`), `session_id` (string, overrides the default
/// `step-<id>` session id (#1436) so a caller's own flow records line up with this
/// dispatch's), `parse_verifiers` (bool, default false — when true,
/// attaches a `failed_verifiers`/`count` field pair, parsed via
/// `parse_failed_verifiers`, onto the returned `StepOutcome`'s companion
/// flow record under `action: "step result"`, `payload.kind:
/// "dispatch.internal"`).
///
/// **A non-zero dispatch exit code is a step-level `Err`, not a silent
/// `Complete`.** The dispatched role's OWN container ran (the darkmux-level
/// dispatch itself always returns `Ok(DispatchResult)`); a non-zero exit
/// means the role's run didn't finish cleanly. Treating that as `Complete`
/// would let downstream `depends_on` steps (e.g. a verify step) run against
/// an incomplete/broken result — this is the same "coder failed, skip
/// downstream steps entirely" contract `mission.coder` always enforced; it
/// is now this kind's DEFAULT for every caller, not a mission-specific
/// carve-out.
///
/// **`config.preserve_dispatch_result` (#1509, opt-in, default `false`).**
/// The `darkmux dispatch` CLI verb's crew-of-one graph
/// (`dispatch_as_crew_of_one`) sets this `true` so it can reconstruct the
/// EXACT pre-#1509 `DispatchResult` (`exit_code`/`stdout`/`stderr`/
/// `session_id`/`out_dir`) the CLI's `--json`, exit-code, and stdout/stderr
/// contract depends on byte-for-byte — the default bail-on-nonzero path
/// above collapses that into one error string and drops `stderr`/`exit_code`
/// entirely, which is fine for a mission-graph step (only Complete/Error
/// matters downstream) but not for a CLI verb whose callers parse the exact
/// shape. When `true`: the non-zero-exit bail above is skipped (a non-zero
/// exit is data, not a step failure — matching the CLI's own pre-#1509
/// semantics, where only a hard `dispatch()`-level `Err` was ever a
/// failure), and `StepOutcome.output` carries a JSON-serialized
/// [`RawDispatchOutcome`] instead of the bare stdout string. Every other
/// caller (mission launch, coder-phase, review) never sets this key, so
/// the key loads as unset and the original behavior is byte-identical.
///
/// Also newly config-driven, same additive/default-preserving shape, so the
/// crew-of-one path can thread the CLI's own `--skip-preflight`/
/// `--max-completion-tokens`/`--json` flags through: `config.skip_preflight`
/// (bool, default `false` — matches every existing caller's hardcoded
/// `false`), `config.max_completion_tokens` (u64, default `None`), and
/// `config.json` (bool, default `true` — matches every existing caller,
/// which always wants the runtime's machine-parseable envelope; the CLI
/// verb is the first caller that ever wants `false`, for its human-readable
/// no-`--json` path).
pub struct DispatchInternalStepKind;

/// (#1509) The full pre-#1509 `crew::dispatch::DispatchResult` shape,
/// packed as `StepOutcome.output` JSON when `config.preserve_dispatch_result`
/// is set — see `DispatchInternalStepKind`'s doc. `dispatch_as_crew_of_one`
/// is the sole consumer today; kept `pub` (not `pub(crate)`) since a
/// `Step.output` on disk is operator-inspectable data, same visibility as
/// `FailedExec`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RawDispatchOutcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<ExecutionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_dir: Option<std::path::PathBuf>,
}

/// (#2480 review, blocker 2) The `Step` + `Task` + upstream-`input` ->
/// [`DispatchOpts`] reconstruction, as a free function.
///
/// `DispatchInternalStepKind::run` used to build this inline, which made the
/// whole reconstruction reachable only through a real `dispatch()` — i.e.
/// only with Docker and a model. That is why `timeout_override_seconds`
/// could sit here hardcoded to `None`, silently undoing `--timeout` on the
/// only path a top-level `darkmux dispatch` takes, with the entire suite
/// green. Extracted so the hop is exercisable by a plain unit test; the
/// caller now has nothing left to drop.
///
/// Every field either comes off `task`/`step.config` or is a deliberate
/// constant with its reason stated in place.
pub(crate) fn dispatch_opts_for(
    step: &Step,
    task: &Task,
    input: &BTreeMap<String, String>,
    ctx: &StepRunCtx,
) -> Result<crate::dispatch::DispatchOpts> {
    use crate::dispatch::{CompactionDispatchArgs, DispatchOpts};

    let cfg: DispatchInternalConfig = load(step, ConfigKind::DispatchInternal)?;
    let role_id = task.role_id.clone().or(cfg.role_id).ok_or_else(|| {
        // The kind id is spelled out rather than read off `self.id()` — this
        // is a free function now, and the literal is the same constant that
        // method returns, so the message is byte-identical to before.
        anyhow!("step `{}`: `dispatch.internal` requires task.role_id or config.role_id", step.id)
    })?;
    let message = compose_message(cfg.message.as_deref().unwrap_or_default(), input);
    let timeout_seconds = cfg.timeout_seconds.map_or(3600, |c| c.saturating_u32());
    let profile_name = task.profile_name.clone().or(cfg.profile_name);
    let image = task.image.clone().or(cfg.image);
    let config_path = cfg.config_path;
    let workdir = task.workdir.clone().or_else(|| cfg.workdir.map(std::path::PathBuf::from));
    // The owning Task names the phase for every step minted from a mission
    // config; the step config's own `phase_id` (the crew-of-one's way of
    // passing the CLI's `--phase`) wins when present. Without the task
    // fallback a config-launched dispatch left with no phase and so no
    // mission on its records: no drill link from the mission view, no
    // events in the sheet, no token attribution (2026-09-04, the grown
    // follow-on steps of a crawl).
    let phase_id = cfg.phase_id.or_else(|| (!task.phase_id.is_empty()).then(|| task.phase_id.clone()));
    // A producer that names the session (the crew-of-one names its ad-hoc
    // dispatch) writes its wire string here; read back strictly. Otherwise
    // the step's own session in this run.
    let session = match cfg.session_id {
        Some(session) => session,
        None => ctx.session(&DispatchInternalStepKind, step)?,
    };
    // (#1509) Additive, default-preserving config passthroughs — see
    // `DispatchInternalStepKind`'s doc. Every existing caller (mission
    // launch, coder-phase, review) never sets these keys, so each falls
    // back to the exact literal the code used to hardcode here.
    let skip_preflight = cfg.skip_preflight.is_some_and(|f| f.0);
    let json = cfg.json.is_none_or(|f| f.0);
    let max_completion_tokens = cfg.max_completion_tokens.and_then(|c| c.as_u32());
    // (#2114 follow-up) `--resume-from <dir>` threaded through the
    // crew-of-one graph's step config (`DispatchAsCrewOfOne::build_graph`)
    // — see that fn's own doc for why the CLI's `DispatchOpts` isn't
    // forwarded wholesale.
    let resume_from = cfg.resume_from.map(std::path::PathBuf::from);
    // (#2295) The finding / mod records the brief carries, read back off
    // the step config. The step config is this list's HOME: the
    // crew-of-one graph writes it from the CLI flags, and a mission graph
    // writes it directly.
    //
    // (#2295 review, CRITICAL 1) Resolution and the APPEND happen HERE,
    // not at the CLI — this is the one point every producer of the field
    // converges on, and appending at the CLI meant a mission graph that
    // set `config.brief_refs` got the read-only mount and the provenance
    // stamp with NO block in its brief and no missing-key refusal. It runs
    // before `dispatch` is called, so a key that addresses no stored
    // record still fails the step before the ack gate and before any
    // container work. The CANONICAL refs (the key as the record spells it)
    // are what get stamped and mounted.
    let brief_refs = cfg.brief_refs.unwrap_or_default();
    let (message, brief_refs) = crate::brief_refs::append_to_brief(
        &message,
        &brief_refs,
        &crate::brief_refs::StoreDirs::resolved(),
    )
    .with_context(|| format!("step `{}`: resolving the brief's records", step.id))?;

    let opts = DispatchOpts {
        finding_sites: None,
        // (#2914) Work never runs on the utility model.
        allow_utility_model: false,
        remote_origin: None,
        live_channel: true,
        brief_refs,
        // (#3074) Read back off the step config the crew-of-one graph wrote
        // it into; a mission step that names no key mounts read-write, as
        // every such step always has.
        workspace_read_only: cfg.workspace_read_only.is_some_and(|f| f.0),
        record_context: None,
        role_id,
        message,
        session,
        timeout_seconds,
        skip_preflight,
        json,
        workdir,
        phase_id,
        machine: None,
        wait: true,
        compaction: CompactionDispatchArgs::default(),
        profile_name,
        config_path,
        force_container: false,
        max_completion_tokens,
        image,
        model_base_url_override: None,
        // (#1483) Stamp the step id so the tailer's live turn/tool/token
        // records attribute to this seat even when the session is not the
        // step's own (a producer named it).
        step_id: Some(step.id.clone()),
        system_prompt_override: None,
        resume_from,
        // (#2153) `dispatch.internal` steps get a fresh tempdir, same
        // as before — no crew-of-one graph step names an exact out
        // dir today.
        host_out: None,
        max_turns_override: None,
        // (#2480 review, blocker 1) `--timeout <n>`, read back off the
        // step config the crew-of-one graph wrote it into. Hardcoding
        // `None` here is what left the flag a no-op on the ONLY path a
        // top-level `darkmux dispatch` takes. A mission/coder-phase/
        // review step names no such key and still resolves `None`, so
        // their standing `env > config > 600` budget is unchanged.
        timeout_override_seconds: cfg.timeout_override_seconds.and_then(|c| c.as_u32()),
    };
    Ok(opts)
}

impl StepKind for DispatchInternalStepKind {
    /// (#2312) An untyped producer: its output is text, not a wrapped body,
    /// so it satisfies only a consumer that asks for [`labels::TEXT`].
    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::data(labels::TEXT)];
        &PORTS
    }

    fn id(&self) -> &'static str {
        ConfigKind::DispatchInternal.id()
    }

    fn display_name(&self) -> &'static str {
        "Dispatch"
    }

    fn run(&self, step: &Step, task: &Task, input: &BTreeMap<String, String>, ctx: &StepRunCtx) -> Result<StepOutcome> {
        use crate::dispatch::dispatch;

        let cfg: DispatchInternalConfig = load(step, ConfigKind::DispatchInternal)?;
        let parse_verifiers = cfg.parse_verifiers.is_some_and(|f| f.0);
        let preserve_dispatch_result = cfg.preserve_dispatch_result.is_some_and(|f| f.0);
        // (#2480 review, blocker 2) The whole `Step`/`Task` ->
        // `DispatchOpts` reconstruction lives in `dispatch_opts_for`, a
        // free function a unit test can call without Docker or a model —
        // so a field can no longer be dropped on this hop while the suite
        // stays green. Everything below is post-dispatch handling.
        let opts = dispatch_opts_for(step, task, input, ctx)?;
        let result = dispatch(opts).map_err(|e| {
            crate::dispatch_internal::with_step_context(e, || format!("step `{}` dispatch.internal", step.id))
        })?;

        // (#1509) `preserve_dispatch_result` callers (the CLI dispatch verb's
        // crew-of-one graph) want the exact `DispatchResult` back, including a
        // non-zero exit code AS DATA — the pre-#1509 CLI never treated a
        // non-zero dispatch exit as a Rust-level failure, only as a different
        // process exit code. Skip the bail-on-nonzero contract below entirely
        // for this opt-in path.
        if preserve_dispatch_result {
            let payload = RawDispatchOutcome {
                exit_code: result.exit_code,
                stdout: result.stdout,
                stderr: result.stderr,
                session_id: result.session_id,
                execution_id: result.execution,
                out_dir: result.out_dir,
            };
            let output = serde_json::to_string(&payload)
                .context("serializing RawDispatchOutcome for preserve_dispatch_result")?;
            return Ok(StepOutcome { output, flow_records: Vec::new(), degraded: None });
        }

        if result.exit_code != 0 {
            anyhow::bail!(
                "step `{}` dispatch.internal: dispatch exited {} — {}",
                step.id,
                result.exit_code,
                result.stdout.trim()
            );
        }

        let mut flow_records = Vec::new();
        if parse_verifiers {
            let failed = parse_failed_verifiers(&result.stdout);
            if !failed.is_empty() {
                flow_records.push(darkmux_flow::FlowRecord {
                    source: Some(darkmux_flow::FlowSource::Scheduler),
                    ..darkmux_flow::FlowRecord::for_session_with(
                        &SessionId::task(ctx.run_id().clone(), &step.task_id),
                        darkmux_flow::Level::Warn,
                        darkmux_flow::Category::Work,
                        darkmux_flow::Stage::Dispatch,
                        darkmux_flow::Payload::StepResult(StepResultPayload {
                            count: Some(failed.len() as u64),
                            failed_verifiers: Some(failed),
                            ..StepResultPayload::new(&step.id, "dispatch.internal")
                        }),
                        step.id.clone(),
                    )
                });
            }
        }

        Ok(StepOutcome {
            output: result.stdout,
            flow_records,
            degraded: None,
        })
    }

    /// (#2394) A local-model dispatch, unless resolution says otherwise —
    /// `resolve_local_seat` reports which of the three it actually is.
    /// The one case this kind decides for itself: no `role_id` at all, on
    /// the Task OR in config. There is nothing to resolve then, so it
    /// claims `LocalModelUnresolved` naming that — `run` will fail on the
    /// same missing field, and the seat says so first.
    fn seat(
        &self,
        step: &Step,
        task: &Task,
        _input: &std::collections::BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> SeatClaim {
        let cfg: DispatchInternalConfig = match load(step, ConfigKind::DispatchInternal) {
            Ok(cfg) => cfg,
            Err(e) => return SeatClaim::LocalModelUnresolved { reason: format!("{e:#}") },
        };
        let Some(role_id) = task.role_id.clone().or(cfg.role_id) else {
            return SeatClaim::LocalModelUnresolved {
                reason: "no role_id on the task or in step config".to_string(),
            };
        };
        let profile_name = task.profile_name.clone().or(cfg.profile_name);
        let config_path = cfg.config_path;
        // NOTE: `step:{id}` here is a gestalt SEAT LABEL (placement-plan
        // diagnostics), NOT a flow-record session id — exempt from the #1436
        // hyphen convention; future colon sweeps should skip it.
        resolve_local_seat(&role_id, profile_name.as_deref(), config_path.as_deref(), &format!("step:{}", step.id))
    }

    /// (#2614 review, MUST FIX + "Also fix" wrong-problem-surfaced finding)
    /// The scheduler-hoisted half of the `--resume-from` checkpoint gate —
    /// see `StepKind::resume_precheck`'s own doc for the full "why here,
    /// why not the workdir check too" reasoning. The early-out on a bare
    /// config load (no `resume_from` key at all) stays cheap and
    /// I/O-free for the overwhelming majority of dispatches that never set
    /// one; only a `--resume-from` dispatch pays for the full
    /// `dispatch_opts_for` hop below.
    ///
    /// **Order matters here.** `refuse_resume_on_bare_hosted_path` runs
    /// FIRST — a role that resolves to the bare hosted single-shot path
    /// (a remote profile + a tool-less role) can never honor a resume
    /// AT ALL, checkpoint content notwithstanding; checking checkpoint
    /// existence/schema first would surface "checkpoint not found" for a
    /// dispatch that was always going to be refused for an entirely
    /// different, more fundamental reason once the operator fixed it. Only
    /// once that's ruled out does `validate_resume_checkpoint_content` run
    /// — the workdir-independent existence/schema/role-match half of the
    /// gate. The workspace/mount-mode half stays inside `dispatch_internal
    /// ::dispatch`'s own `validate_resume_checkpoint` call, which runs
    /// later (post-wave-load) once this step's real workspace is resolved;
    /// that later call re-validates the content half too (a harmless
    /// second no-op read of the same file), same "duplicate read, not
    /// duplicate authority" pattern the CLI wrapper's now-deleted hoist
    /// relied on.
    fn resume_precheck(
        &self,
        step: &Step,
        task: &Task,
        input: &std::collections::BTreeMap<String, String>,
        ctx: &StepRunCtx,
    ) -> Result<()> {
        let cfg: DispatchInternalConfig = load(step, ConfigKind::DispatchInternal)?;
        if cfg.resume_from.is_none() {
            return Ok(());
        }
        let opts = dispatch_opts_for(step, task, input, ctx)
            .with_context(|| format!("step `{}` dispatch.internal", step.id))?;
        let Some(resume_from) = opts.resume_from.clone() else {
            return Ok(());
        };
        crate::dispatch_internal::refuse_resume_on_bare_hosted_path(&opts)?;
        crate::dispatch_internal::validate_resume_checkpoint_content(&resume_from, &opts.role_id)?;
        Ok(())
    }
}

/// Wraps `single_shot::single_shot_chat` (local LMStudio) /
/// `single_shot_chat_hosted` (a remote OpenAI-compatible endpoint) — one
/// container-free chat-completions call, no agent loop. Required
/// `Step.config` keys: `model` (string), `user` (string, the base user
/// message — prior-dependency output is prepended per `compose_message`).
/// Optional: `system` (string, default empty), `temperature` (f32,
/// default 0.7, LOCAL dialect only), `max_tokens` (u32, default 4096),
/// `timeout_seconds` (u32, default 120), `endpoint`
/// (`darkmux_types::ModelEndpoint` JSON — presence selects the HOSTED
/// dialect instead of local).
///
/// **Hosted-arm budgets (#1412, #2902 step 5).** The LOCAL dialect
/// (LMStudio) is never metered. The HOSTED arm passes the endpoint's
/// rolling-window budget (`crate::budget::admit_endpoint`) and reserves
/// against this call's dispatch cap (a fresh `DispatchBudget` from
/// `limits.tokens_per_dispatch`) before the call, and settles the cap with
/// the call's real spend after it. Neither refuses or clamps a call: a
/// breach warns, and an endpoint `wait` holds the call until there is room.
pub struct DispatchSingleShotStepKind;

/// (#1444 review) The hosted `dispatch.single_shot` step's "step result"
/// payload, extracted as a pure function so its token block is testable.
///
/// It was an inline `json!` inside the hosted arm of
/// `DispatchSingleShotStepKind::run`, which performs a real HTTP call with
/// no override seam — so nothing in the suite ever built it, and replacing
/// `reply.reasoning_tokens` with `Null` there left all 1503 crew tests
/// green. Same pure-payload/emitter division `map_item_token_payload` and
/// `turn_tokens_payload` already use.
///
/// The token fields are the call's usage counts, the ones its usage record
/// carries: `null` for a field the endpoint never reported, never a
/// fabricated `0`, and `total_tokens` by the one total rule. Whether
/// reasoning sits inside `completion_tokens` is provider-scoped (see
/// `darkmux_trajectory::UsageCounts::reasoning`).
fn hosted_single_shot_step_payload(
    step_id: &str,
    budget: Option<u64>,
    max_tokens_requested: u32,
    max_tokens_sent: u32,
    reply: &crate::single_shot::SingleShotReply,
) -> StepResultPayload {
    let counts = &reply.counts;
    StepResultPayload {
        // (#2902 step 5, CLAUDE.md contract 8: the wire keeps its historical
        // spelling) The per-dispatch cap, under the key v3.13.0 shipped.
        tokens_per_dispatch: budget,
        max_tokens_requested: Some(u64::from(max_tokens_requested)),
        max_tokens_sent: Some(u64::from(max_tokens_sent)),
        prompt_tokens: counts.prompt,
        completion_tokens: counts.completion,
        total_tokens: counts.total_tokens(),
        // (#1444, payload-additive — FLOW_SCHEMA_VERSION 1.44.0)
        reasoning_tokens: counts.reasoning,
        cached_tokens: counts.cached,
        ..StepResultPayload::new(step_id, "dispatch.single_shot")
    }
}

/// The contract-#2 bookend records of ONE role execution a Tier-1 step kind
/// runs (`dispatch.single_shot`, and each item of `dispatch.map`): keyed on
/// the task session the step's `step result` records already use, so a
/// consumer joins the pair to the tokens, and stamped with the execution.
///
/// The savings hero reads `payload.endpoint` off these bookends and off
/// nothing else, so hosted spend is attributed to the endpoint the call went
/// to. `endpoint_label` is `None` for a local call, which leaves the payload
/// byte-identical to a purely-local dispatch's: the key is simply absent.
struct ExecutionBookends<'a> {
    /// The step kind's id, stamped as the payload's `kind`.
    kind: &'static str,
    session: &'a SessionId,
    execution: &'a ExecutionId,
    step: &'a Step,
    model: &'a str,
    endpoint_label: Option<&'a str>,
}

impl ExecutionBookends<'_> {
    /// This execution's record carrying `payload` (whose action is the
    /// record's) at `level`.
    fn record(&self, level: darkmux_flow::Level, payload: darkmux_flow::Payload) -> darkmux_flow::FlowRecord {
        darkmux_flow::FlowRecord {
            source: Some(darkmux_flow::FlowSource::Scheduler),
            model: Some(self.model.to_string()),
            ..darkmux_flow::FlowRecord::for_execution_with(
                self.session,
                self.execution,
                level,
                darkmux_flow::Category::Work,
                darkmux_flow::Stage::Dispatch,
                payload,
                self.step.id.clone(),
            )
        }
    }

    /// A `dispatch.start` payload naming this execution's step and kind, and
    /// the hosted endpoint it calls (absent for a local call).
    fn start_payload(&self, item_index: Option<u64>) -> DispatchStartPayload {
        DispatchStartPayload {
            step_id: Some(self.step.id.clone()),
            kind: Some(self.kind.to_string()),
            item_index,
            endpoint: self.endpoint_label.map(str::to_string),
            ..Default::default()
        }
    }

    /// A terminal payload naming this execution's step and kind, and the
    /// hosted endpoint it called, for the caller to fill in.
    fn end_payload(&self, total_turns: u64) -> DispatchEndPayload {
        self.stamp(DispatchEndPayload::new(total_turns))
    }

    /// `payload` naming this execution's step, kind and endpoint.
    fn stamp(&self, payload: DispatchEndPayload) -> DispatchEndPayload {
        DispatchEndPayload {
            step_id: Some(self.step.id.clone()),
            kind: Some(self.kind.to_string()),
            endpoint: self.endpoint_label.map(str::to_string),
            ..payload
        }
    }

    /// Open the execution's liveness edge and arm its terminal: the
    /// `dispatch.start` names the map item (`item_index`) when it is one, and
    /// dropping the guard without `close` (a `?`, a panic) writes a
    /// `dispatch.error` saying `abort_message`.
    fn open<'c>(&self, ctx: Option<&'c StepRunCtx>, item_index: Option<u64>, abort_message: &str) -> StepBookend<'c> {
        let abort = DispatchEndPayload {
            item_index,
            result_class: Some(ResultClass::Error),
            error: Some(abort_message.to_string()),
            ..self.end_payload(0)
        };
        StepBookend::new(
            ctx,
            self.record(darkmux_flow::Level::Info, darkmux_flow::Payload::DispatchStart(self.start_payload(item_index))),
            self.record(darkmux_flow::Level::Error, darkmux_flow::Payload::DispatchError(abort)),
        )
    }
}

/// (#2925) A hosted `dispatch.single_shot` call failed: under the one hosted
/// error policy ([`crate::dispatch_internal::call_may_have_spent`]) a call the
/// endpoint may have processed is counted (an `absent` usage record) and
/// charged against the dispatch cap like a reply with no usage; any other
/// failure spends nothing. `step_and_endpoint` is the step id and the
/// endpoint label its records carry.
fn charge_failed_hosted_step_call(
    err: &anyhow::Error,
    req: &crate::single_shot::HostedSingleShotRequest<'_>,
    bucket: &Mutex<DispatchBudget>,
    caller: &crate::budget::BudgetCaller<'_>,
    ctx: Option<&StepRunCtx>,
    step_and_endpoint: (&str, &str),
) {
    if !crate::dispatch_internal::call_may_have_spent(err) {
        return;
    }
    let (step_id, endpoint_label) = step_and_endpoint;
    let usage = crate::dispatch::build_telemetry_record(
        darkmux_flow::Level::Info,
        darkmux_flow::FlowSource::Tokens,
        step_id,
        caller.session,
        caller.execution,
        Some(req.model),
        None,
        darkmux_flow::Payload::TelemetryTokens(crate::usage::usage_payload(
            &crate::usage::CallFacts {
                call_kind: crate::usage::CallKind::SingleShot,
                role_id: None,
                requested_model: req.model,
                reported_model: None,
                endpoint: endpoint_label,
                endpoint_id: req.endpoint.named_id(),
            },
            &darkmux_trajectory::UsageCounts::default(),
        )),
    );
    match ctx {
        Some(c) => c.emit(usage),
        None => {
            let _ = darkmux_flow::record(usage);
        }
    }
    if let Ok(body) = req.body() {
        crate::budget::settle_dispatch_live(
            bucket,
            crate::dispatch_internal::unanswered_hosted_spend(err, req.max_tokens, &body),
            step_id,
            caller,
        );
    }
}

impl StepKind for DispatchSingleShotStepKind {
    fn id(&self) -> &'static str {
        ConfigKind::DispatchSingleShot.id()
    }

    fn display_name(&self) -> &'static str {
        "Dispatch (single-shot)"
    }

    /// (#1979) Task-scoped, NOT the trait default's step scope. Deliberate:
    /// sibling seats fanned out within one task share this key so a
    /// consumer can join a seat's tokens to its endpoint (see the record
    /// built in this kind's own dispatch path). The step remains individually attributable through
    /// `payload.step_id` and `handle` — grouping and identity are different
    /// jobs, and this field is the grouping one.
    fn session_scope(&self) -> SessionScope {
        SessionScope::Task
    }


    /// (#2394) A single call against ONE named model: hosted when
    /// `config.endpoint` is present, local otherwise. The local arm claims
    /// [`SeatClaim::LocalModel`] only when this step carries the residency
    /// hints a wave load needs (`model` + `n_ctx`); a launcher that stamped
    /// neither leaves nothing to place, so the claim is
    /// [`SeatClaim::LocalModelUnresolved`] naming the missing field rather
    /// than a silent fall-through onto the hosted track.
    ///
    /// Local seats are stamped by every production launcher; a bare
    /// hand-written `dispatch.single_shot` step with only `model` set is
    /// the case that warns, and it warns because it genuinely runs
    /// unleased.
    fn seat(
        &self,
        step: &Step,
        _task: &Task,
        _input: &BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> SeatClaim {
        // (#2902 step 3) An UNMANAGED `config.endpoint` is the hosted track;
        // a managed one (or none) is placed locally. One that cannot be
        // resolved keeps the hosted claim it always had: `run` refuses it
        // with the reason.
        let cfg: SingleShotConfig = match load(step, ConfigKind::DispatchSingleShot) {
            Ok(cfg) => cfg,
            Err(e) => return SeatClaim::LocalModelUnresolved { reason: format!("{e:#}") },
        };
        let call = &cfg.call;
        match step_endpoint(call) {
            Ok(None) => {}
            Ok(Some(ep)) => return SeatClaim::UnmanagedEndpoint(crate::step_kinds::EndpointSlot::of(&ep)),
            Err(_) => return SeatClaim::UnmanagedEndpoint(crate::step_kinds::EndpointSlot::unresolved()),
        }
        let Some(min_ctx) = call.n_ctx.and_then(|n| n.as_u32()) else {
            return SeatClaim::LocalModelUnresolved {
                reason: format!("local model `{}` has no usable config.n_ctx", call.model),
            };
        };
        SeatClaim::LocalModel(darkmux_gestalt::Placement {
            model_key: call.model_key.clone().unwrap_or_else(|| call.model.clone()),
            identifier: local_dispatch_wire_model_id(call),
            min_ctx,
            seat: format!("step:{}", step.id),
        })
    }

    fn run(&self, step: &Step, task: &Task, input: &BTreeMap<String, String>, ctx: &StepRunCtx) -> Result<StepOutcome> {
        self.run_single_shot(step, task, input, ctx)
    }
}

impl DispatchSingleShotStepKind {
    fn run_single_shot(
        &self,
        step: &Step,
        task: &Task,
        input: &BTreeMap<String, String>,
        run_ctx: &StepRunCtx,
    ) -> Result<StepOutcome> {
        use crate::single_shot::HostedSingleShotRequest;
        // Records go out live through the scheduler's emitter when there is
        // one, else batch into the outcome (see `StepBookend`).
        let ctx = run_ctx.live();
        let session = &run_ctx.session(self, step)?;
        // One model call, one execution: its bookends, its usage record and
        // the budget records of its gate all name it.
        let execution = &ExecutionId::mint();

        let cfg: SingleShotConfig = load(step, ConfigKind::DispatchSingleShot)?;
        let call = &cfg.call;
        // (#2570) The identifier this step actually ADDRESSES: the bare
        // `config.model` string for a hosted step (an endpoint deployment
        // name — darkmux never loads it into local residency), or the SAME
        // darkmux-namespaced identifier `seat()` derived above for a local
        // one. Pre-#2570 every record below (and the local dispatch itself)
        // used the bare `model` unconditionally, so a local step with no
        // `config.identifier` override loaded `darkmux:<key>` and addressed
        // `<key>` — the #2240/#2536 split, here. Computed once so the
        // records and the actual call can never disagree about what was
        // dispatched.
        let endpoint = step_endpoint(call).with_context(|| format!("step `{}`: config.endpoint", step.id))?;
        let managed_endpoint =
            step_managed_endpoint(call).with_context(|| format!("step `{}`: config.endpoint", step.id))?;
        let is_hosted = endpoint.is_some();
        let wire_model = step_wire_model(call, is_hosted);
        let system = call.system.as_deref().unwrap_or("");
        let user = compose_message(cfg.user.as_deref().unwrap_or_default(), input);
        let max_tokens = call.max_tokens.map_or(4096, |c| c.saturating_u32());
        let timeout_seconds = call.timeout_seconds.map_or(120, |c| c.saturating_u32());

        // (#2344) Contract #2's liveness bookends, which this kind owed and
        // never emitted — it performs REAL model work (one chat completion,
        // local or hosted) and emitted only its own `step result` vocabulary,
        // so a `dispatch.single_shot` seat had tokens and a model but no
        // start and no terminal anywhere. Strictly worse than `dispatch.map`,
        // which at least had the pair. Opened BEFORE any model work and
        // closed on every exit path, `?` and panic included, via
        // `StepBookend`'s Drop.
        let endpoint_label: Option<String> =
            endpoint.as_ref().map(|ep| crate::target::endpoint_route_label(ep, wire_model.as_ref()));
        let records = ExecutionBookends {
            kind: "dispatch.single_shot",
            session,
            execution,
            step,
            model: wire_model.as_ref(),
            endpoint_label: endpoint_label.as_deref(),
        };
        let mut bookend = records.open(ctx, None, "dispatch.single_shot terminated before completion (early return or panic)");

        // (#2344) Session-liveness heartbeat — the same in-process twin of
        // the container path's emitter (#638) `dispatch.map` grew, opened at
        // the same point the bookends open and keyed on the SAME task session
        // every record on this path uses. One hosted
        // single-shot against a reasoning model is minutes of real
        // wall-clock; without a beat none of it was visible on the live
        // fleet view, because bookends are terminal-only records. Stopped
        // explicitly once the call returns and before the terminal record
        // below; for a `?`/panic in between, `SessionEmitter::drop` (#2344)
        // now removes the presence key itself, the same DEL `stop()` issues,
        // instead of only halting the beat thread and leaving the TTL to
        // age the key out.
        //
        // See `DispatchMapStepKind::run_map`'s own spawn site for why the
        // key is TASK-scoped here rather than step-scoped.
        let mut session_emitter =
            darkmux_flow::session_presence::spawn_session_emitter(session, None, Some(wire_model.to_string()));

        let mut flow_records = Vec::new();

        let call_started = std::time::Instant::now();
        // Who this call's budget records are about, for both arms.
        let budget_caller = crate::budget::BudgetCaller {
            session,
            execution,
            role_id: None,
            model: Some(wire_model.as_ref()),
            phase_id: Some(&task.phase_id),
            profiles_file: call.config_path.as_deref(),
        };
        // (#3121) A failed call ends the execution with ITS error: the
        // terminal names it (a signal, a refused budget, an endpoint error)
        // instead of the guard's "early return or panic".
        let attempt = match &endpoint {
            Some(endpoint) => hosted_single_shot_reply(
                step,
                &HostedSingleShotRequest {
                    endpoint,
                    model: wire_model.as_ref(),
                    system,
                    user: &user,
                    max_tokens,
                    timeout_seconds,
                },
                endpoint_label.as_deref(),
                session,
                &budget_caller,
                ctx,
                &mut flow_records,
            ),
            None => local_single_shot_reply(
                step,
                managed_endpoint.as_ref(),
                call,
                wire_model.as_ref(),
                system,
                &user,
                (max_tokens, timeout_seconds),
                &budget_caller,
            ),
        };
        let reply = match attempt {
            Ok(reply) => reply,
            Err(e) => {
                if let Some(em) = session_emitter.take() {
                    em.stop();
                }
                bookend.fail(&e, call_started.elapsed().as_millis() as u64);
                return Err(e);
            }
        };

        // (#2902 step 1a) The one usage record for this one model call, from
        // the shared reply seam, under the SAME handle/session/mission as
        // this step's bookends (so it joins their run). Live through the
        // scheduler's seam when streaming, like `dispatch.map`'s per-item
        // record; batched into the outcome otherwise.
        let usage_endpoint =
            endpoint_label.clone().unwrap_or_else(|| crate::usage::lmstudio_endpoint(None));
        let usage_record = crate::dispatch::build_telemetry_record(
            darkmux_flow::Level::Info,
            darkmux_flow::FlowSource::Tokens,
            &step.id,
            session,
            execution,
            Some(wire_model.as_ref()),
            None,
            // A step runs no role (#2914: `None` → the call's `purpose` is decided
            // by its kind alone).
            darkmux_flow::Payload::TelemetryTokens(reply.usage_payload(
                crate::usage::CallKind::SingleShot,
                None,
                wire_model.as_ref(),
                &usage_endpoint,
                endpoint.as_ref().or(managed_endpoint.as_ref()).and_then(|ep| ep.named_id()),
            )),
        );
        match ctx {
            Some(c) => c.emit(usage_record),
            None => flow_records.push(usage_record),
        }

        // (#2344) The one call is over — no model work is in flight for this
        // step — so stop the heartbeat before the terminal record, the same
        // ordering `dispatch.map` and the container path both use.
        if let Some(em) = session_emitter.take() {
            em.stop();
        }
        bookend.close(records.record(
            darkmux_flow::Level::Info,
            darkmux_flow::Payload::DispatchComplete(records.stamp(crate::dispatch_internal::single_call_complete(
                &crate::dispatch_envelope::DirectTokens::of(&reply.counts),
                call_started.elapsed().as_millis() as u64,
                reply.content.len() as u64,
            ))),
        ));

        Ok(StepOutcome {
            output: reply.content,
            flow_records,
            degraded: None,
        })
    }
}

// ─── dispatch.map (#1442) ───────────────────────────────────────────────

// (#3035) Each `dispatch.map` item is one dispatch, so each hosted item gets
// its own `DispatchBudget` (`crate::dispatch_budget`) from its endpoint's
// `limits.tokens_per_dispatch`. The pre-5.0 step-level allowance shared
// across a step's items by `bucket_group` is gone: a whole-run budget is the
// endpoint's rolling `limits.window`.


/// (#1442) One `dispatch.map` item's outcome, serialized (in input-collection
/// order) into the step's `output` JSON array. A downstream step reads this
/// array back. `ok == false` marks an ISOLATED per-item failure — the loop
/// CONTINUED past it (see [`DispatchMapStepKind`]'s error policy) rather than
/// failing the whole step; `error` names the failure (a dispatch error, or a
/// remote-budget skip). `content` is the reply text on success (empty on a
/// skip/error).
///
/// **Per-item telemetry (#1442, probe-reconstruction envelope honesty).** Two
/// fields carry the per-item observability the probe rewiring's reconstruction
/// needs:
/// - `served_model` — the model the ENDPOINT reported it actually served, for a
///   HOSTED item only (from the reply body's `model` field, mirroring how
///   [`crate::single_shot::SingleShotReply::model`] surfaces it and how the
///   review pipeline's member records capture it). A LOCAL item sets it `None`
///   by construction — `lms ps` is the only ground truth for a local dispatch,
///   never what LMStudio happens to echo back — matching the established
///   `served_model = if endpoint.is_some() { reply.model } else { None }`
///   semantics in `darkmux-lab`'s `review.rs`. A missing served model is `None`,
///   NEVER an empty string and NEVER the requested model echoed back as if
///   served.
/// - `wall_ms` — the CUMULATIVE wall-clock spent dispatching this item,
///   accumulated across every attempt (the same accounting shape `total_tokens`
///   already uses: a `retry_on_empty` retry adds its own call's elapsed on top,
///   just as it adds its own tokens). An item skipped before any call fired (a
///   first-attempt remote-budget exhaustion) measures the honest near-zero of
///   the skip — a real measured `0`-ish duration, not a fabricated value.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MapItemResult {
    pub index: usize,
    pub ok: bool,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// (#1530 dogfood) The `usage.prompt_tokens` / `usage.completion_tokens`
    /// split alongside the total. `SingleShotReply` has carried both since
    /// #1361 — added there specifically so callers could emit a
    /// `telemetry.tokens` record the fleet dashboard's `tokensOffMeter()`
    /// can classify, since that function reads the SPLIT fields and only
    /// sums `total_tokens` into the headline.
    ///
    /// They were dropped on the way through this struct when the review
    /// pipeline's probe/verify stages migrated onto `dispatch.map` (#1442),
    /// which made every map-dispatched call headline-visible but
    /// classification-invisible: the 2.3.0 dogfood measured 62,047 of
    /// 152,271 local tokens (41%) landing in no chip at all, because
    /// `GENERATED` reads `completion_tokens` and `fresh`/`re-read`/
    /// `unclassified` read `prompt_tokens`, and both were null.
    ///
    /// Still `Option` — a provider that reports only a total leaves these
    /// `None` and the emitter below omits them rather than inventing a
    /// split (the no-fabrication rule that governed the original code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    /// (#1444 review) `usage.completion_tokens_details.reasoning_tokens` and
    /// `usage.prompt_tokens_details.cached_tokens`, accumulated across an
    /// item's attempts the same way the split above is.
    ///
    /// #1444's first pass added both to `SingleShotReply` and then dropped
    /// them HERE, so `dispatch.map` — the highest-volume hosted-remote path
    /// in the product, and the one the review funnel's probe and verify
    /// stages run on — emitted a `telemetry.tokens` record with no reasoning
    /// burn in it at all, while the 1.44.0 schema entry claimed
    /// `telemetry.tokens` coverage. Exactly the class of loss #1530 closed
    /// for the prompt/completion split, one field-pair later.
    ///
    /// Still `Option`, with the same honesty rule as every sibling: a
    /// provider that never named the field leaves it `None`, and
    /// the usage writer omits the key rather than inventing a
    /// zero. Each field is summed independently ([`MapItemResult::tokens_of`]):
    /// a provider can name one details object without the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_model: Option<String>,
    #[serde(default)]
    pub wall_ms: u64,
    /// (#1605) How many `retry_on_error` attempts this item actually
    /// consumed (0 on a first-attempt success or a non-retried error — the
    /// default and overwhelmingly common case). Distinct from
    /// `retry_on_empty`'s bookkeeping (which never surfaces per-item,
    /// because an all-empty item still ends `ok: true` and its retries are
    /// invisible-by-design): an ERROR retry is loud on purpose — darkmux#1605
    /// found that "every probe draw errored" reads identically whether it
    /// was a clean single failure or a transient blip that self-healed on
    /// retry, so callers that care (the review probe stage) sum this into
    /// `ReviewEnvelope::probe_retries` to make a recovered run visibly
    /// different from one that never needed to retry at all.
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub retried: u32,
}

fn u32_is_zero(n: &u32) -> bool {
    *n == 0
}

/// The text a `{item}` placeholder in `dispatch.map`'s `user_template` is
/// replaced with: a JSON-string item substitutes verbatim (no surrounding
/// quotes); any other JSON value substitutes its compact serialization. Zero
/// domain knowledge — the collection is data, the substitution is mechanical.
fn map_item_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// (#2310 P1 review finding I1) An item MAY be a `{"system": <string>,
/// "item": <value>}` object — an OPTIONAL per-item persona override, for a
/// caller whose collection is minted at RUN TIME from data the step-level
/// `config.system` (fixed at BUILD time) cannot see. Zero domain knowledge
/// added: this is still config-driven (the SHAPE of an item, not what it
/// means), matching this kind's Tier 1 classification — see its own doc.
/// The override shape is EXACT (round-2 review of #2339): an object with
/// exactly the two keys `system` and `item`, and `system` a string. Anything
/// else — a bare string, a number, an object with only one of the keys, an
/// object with the two keys plus extras, or a non-string `system` — is the
/// plain item as before: no override, and the whole value substitutes via
/// [`map_item_text`] under the step's own `config.system`. Exactness is what
/// keeps a legitimate collection item that merely CONTAINS those keys from
/// being hijacked.
///
/// Returns `(system_override, payload)` — `payload` is the `"item"` field
/// when the shape matched, else the item unchanged (so the two never disagree
/// about which shape won).
fn item_system_and_payload(item: &serde_json::Value) -> (Option<&str>, &serde_json::Value) {
    match item.as_object() {
        Some(obj) if obj.len() == 2 => match (obj.get("system").and_then(|v| v.as_str()), obj.get("item")) {
            (Some(system), Some(payload)) => (Some(system), payload),
            _ => (None, item),
        },
        _ => (None, item),
    }
}

/// Resolve a `dispatch.map` step's collection. Precedence: an explicit
/// `config.collection` JSON array wins; otherwise the collection is a RUNTIME
/// INPUT — the dependency output named by `config.collection_input`, or
/// (#2310 P2a) when `step` is NOT the first step of `task`, its
/// immediately-previous same-task step's output (the one thing a later
/// step's collection can sensibly default to — see below), or (when neither
/// of those apply and the step has exactly one dependency input) that one
/// input.
///
/// **Why the predecessor gets priority over the single-input rule (#2310
/// P2a).** Before #2310 P2a, a non-first step in a multi-step Task received
/// ONLY its predecessor's output — one entry, so the old single-input
/// fallback always resolved it. `scheduler::gather_inputs` now ALSO chains
/// in the Task's own `depends_on`/`reads` outputs for every step (see that
/// fn's doc), so a non-first map step whose Task also declares a `reads`
/// can legitimately see two or more inputs even though there is still only
/// one sensible collection source: its predecessor. Falling straight
/// through to the old "two-or-more-without-`collection_input`-is-a-loud-
/// error" rule here would break exactly this shape (`review.json`'s four
/// probe/verify tasks, none of which stamp `collection_input`) — so a
/// non-first step tries its predecessor's key FIRST, before the ambiguity
/// check. A first step has no predecessor to prefer, so its two-or-more
/// case stays exactly as loud as before.
///
/// **Loud on every typo-shaped source (#1442 gate — the #1418 class: a
/// silent empty masks a config typo as a clean no-op run):**
/// - a present-but-non-array `config.collection` is a loud `Err`, never a
///   silent fall-through to input resolution;
/// - a `collection_input` naming a key ABSENT from `input` is a loud `Err`
///   naming the missing key AND the inputs actually present;
/// - (a non-first step whose predecessor produced no `input` entry falls
///   through to the rules below, same as a first step — it does NOT error
///   just because a preferred key was absent);
/// - two or more dependency inputs with NO `collection_input` AND no usable
///   predecessor entry is a loud `Err` ("name one") — returning empty there
///   would not be a refusal to guess, it would BE a guess ("there is no
///   collection");
/// - a present-but-non-array INPUT source is a loud `Err` too.
///
/// What stays a REAL empty (and short-circuits cleanly): a genuinely absent
/// source (no `collection` key, no `collection_input`, no predecessor entry,
/// zero dependency inputs) and a present-but-blank source string (the
/// upstream truly produced nothing). `run` surfaces every `Err` here;
/// `residency` stays best-effort and swallows them (see its doc).
fn resolve_map_collection(
    step: &Step,
    task: &Task,
    input: &BTreeMap<String, String>,
) -> Result<Vec<serde_json::Value>> {
    let source: MapSource = load(step, ConfigKind::DispatchMap)?;
    if let Some(items) = source.collection {
        return Ok(items);
    }
    let present_keys = || {
        if input.is_empty() {
            "none".to_string()
        } else {
            input.keys().cloned().collect::<Vec<_>>().join(", ")
        }
    };
    // (#2310 P2a) The predecessor step's output, when `step` is NOT the
    // first step of `task` and that predecessor actually produced an
    // `input` entry (it may not have, e.g. no recorded output) — preferred
    // over the single-input/ambiguity rules below.
    let predecessor_source: Option<&String> = task
        .step_ids
        .iter()
        .position(|id| id == &step.id)
        .filter(|&i| i > 0)
        .and_then(|i| task.step_ids.get(i - 1))
        .and_then(|prev_id| input.get(prev_id));
    let source: Option<&String> = match source.collection_input.as_deref() {
        Some(key) => match input.get(key) {
            Some(s) => Some(s),
            None => bail!(
                "step `{}`: `dispatch.map` config.collection_input names `{key}`, which is \
                 not among this step's dependency inputs (present: {})",
                step.id,
                present_keys()
            ),
        },
        None if predecessor_source.is_some() => predecessor_source,
        None if input.len() == 1 => input.values().next(),
        None if input.len() > 1 => bail!(
            "step `{}`: `dispatch.map` has two or more dependency inputs ({}) and no \
             config.collection_input — name which input carries the collection",
            step.id,
            present_keys()
        ),
        None => None,
    };
    let Some(source) = source else {
        return Ok(Vec::new());
    };
    let source = source.trim();
    if source.is_empty() {
        return Ok(Vec::new());
    }
    let val: serde_json::Value = serde_json::from_str(source).with_context(|| {
        format!("step `{}`: `dispatch.map` collection input is not valid JSON", step.id)
    })?;
    match val {
        serde_json::Value::Array(items) => Ok(items),
        _ => bail!(
            "step `{}`: `dispatch.map` collection input must be a JSON array",
            step.id
        ),
    }
}

/// (#1442) `dispatch.map` — ONE single-shot dispatch PER ITEM of a runtime
/// input collection. The generic building block the review pipeline's
/// probe/verify stages restructure onto (#1442): where
/// [`DispatchSingleShotStepKind`] wraps exactly ONE chat-completions call
/// driven by upstream `Step.output`, `dispatch.map` wraps a whole FOR-EACH
/// loop over a runtime-derived collection — a count not known at graph-build
/// time (a diff's bundle count, a judge's confirmed-finding count). Static
/// per-item graph tasks are therefore impossible; runtime-count iteration
/// inside ONE step is exactly what the #1352 tiering doctrine permits.
///
/// **Tier 1, config-driven, zero domain knowledge (#1352).** Every parameter
/// reads from `Step.config`; the collection is DATA, not code; there is no
/// caller-supplied strategy (which is what would make it a Tier 2 pattern).
/// It is [`DispatchSingleShotStepKind`]'s sibling that ITERATES — the same
/// LOCAL/HOSTED dialect split, the same per-item `max_tokens` clamp, the same
/// per-item record shape — with a per-item loop and a per-item token cap
/// bucket ([`DispatchBudget`]) added on top. That there is no genuinely-new
/// *pluggable algorithm* (only a new outer loop shape over existing
/// primitives) is why it lands in `builtins` and not `patterns/`.
///
/// Required `Step.config`: `model` (string), `user_template` (string — its
/// `{item}` placeholder is replaced per item by [`map_item_text`]). Optional:
/// `collection` (JSON array — items inline; else the runtime input, see
/// [`resolve_map_collection`]), `collection_input` (string — which dependency
/// input carries the collection), `system` (string, default empty),
/// `max_tokens` (u32, default 4096), `temperature` (f32, default 0.7, LOCAL
/// only), `timeout_seconds` (u32, default 120), `endpoint`
/// (`darkmux_types::ModelEndpoint` JSON — presence selects the HOSTED
/// dialect), `n_ctx`/`identifier` (residency hints — see [`Self::residency`]),
/// `retry_on_empty` (u32, default 0 — see below), `retry_on_error` (u32,
/// default 0 — see below).
///
/// **Per-item `system` override (#2310 P1 review finding I1).** An item MAY
/// be a `{"system": <string>, "item": <value>}` object instead of a plain
/// value — that item's dispatch uses ITS OWN `system`, not `config.system`,
/// while the OTHER items in the same collection keep using the step's own
/// (unaffected). For a caller whose collection is minted at RUN TIME (a
/// render step's `Step.output`) from a persona the BUILD-TIME `config.system`
/// stamp cannot see. See [`item_system_and_payload`]'s own doc.
///
/// **`retry_on_empty` (#1442, the generic port of the probe stage's
/// retry-on-empty loop).** Default `0` (off) — a call whose trimmed content
/// comes back empty is accepted as-is (`ok: true`, empty `content`). When set
/// to `N > 0`, an empty-content reply is RE-DISPATCHED up to `N` additional
/// times (so `N = 1` matches the review probe's historical single retry: up
/// to 2 attempts total), stopping early the moment a non-empty reply lands.
/// Tokens are accumulated across EVERY attempt (an empty reasoning-model reply
/// still burns — and is billed — its whole completion budget), and the hosted
/// arm draws from the item's token cap on each attempt (a retry is another
/// billable call). The block stays Tier-1-pure and domain-blind:
/// `retry_on_empty` is a plain config integer, not review-specific knowledge.
///
/// **`retry_on_error` (#1605, darkmux issue #1605 cause 2 — "every probe
/// draw errored").** Default `0` (off) — a dispatch-level `Err` on any
/// attempt is NOT retried, matching this block's ORIGINAL policy: the
/// single-shot primitive already owns its own transport backoff (a bounded
/// 429/503 ladder), so a second-guessing retry on top would hide a real
/// infra problem for every caller by default. It isolates as `ok: false`
/// exactly as before. When a step explicitly opts in with `N > 0`, an
/// errored attempt is RE-DISPATCHED up to `N` additional times, each
/// separated by a short fixed backoff
/// ([`RETRY_ON_ERROR_BACKOFF`]) — for the ONE caller
/// darkmux#1605 found this genuinely warranted (a batch of probe draws
/// failing together reads like a transient endpoint-side outage, not a
/// structural break), never for anything downstream of a successful probe
/// (judge/verify stay at the default). [`MapItemResult::retried`] records
/// how many error-retries an item actually consumed, so a recovered item is
/// visibly distinct from one that never needed to retry. Still Tier-1-pure:
/// the RETRY POLICY is a plain config integer a caller opts into; nothing
/// here knows what "probe" or "review" means.
///
/// **Templating boundary (by design, #1442).** `user_template` can reference
/// ONLY `{item}` — never another dependency's output. A consumer that needs
/// richer per-item prompts pre-renders each full prompt UPSTREAM and passes
/// the rendered strings as the collection items themselves, with
/// `user_template: "{item}"` verbatim. This is deliberate foreclosure: the
/// moment this block learns to weave other inputs into a template it starts
/// growing mission-specific templating (the review pipeline being the
/// obvious tempter), and it stops being a Tier 1 generic block.
///
/// **Per-item error isolation (the defined policy).** A dispatch error for
/// ONE item is captured into that item's [`MapItemResult`] (`ok: false`,
/// `error` set) and the loop CONTINUES — one bad item never kills its
/// siblings, and the step returns `Ok` with an array recording each outcome.
/// This mirrors the probe stage's "aggregate, never discard" contract (a
/// failed draw must not lose the other draws' findings); a caller wanting
/// fail-fast inspects the `ok: false` entries. (Contrast
/// [`DispatchInternalStepKind`], where a non-zero agentic dispatch exit is a
/// step-level `Err` — that's a single-dispatch step with downstream
/// `depends_on` to protect; a map's whole point is surviving partial failure.)
///
/// **Empty-collection short-circuit.** An empty collection is a completed
/// no-op: [`Self::residency`] returns `None` (so the wave loader never loads
/// a model the step won't use — the #1442 property ported generically from
/// the review verify seat's empty-docket short-circuit), and `run` returns
/// `Ok` with an empty `[]` output and a named short-circuit record before any
/// dispatch. This makes the short-circuit a property of the BLOCK.
pub struct DispatchMapStepKind;

impl DispatchMapStepKind {
    /// The seat of a step whose config does not load: no model is claimed
    /// for an empty collection (which `run` completes without one), and
    /// anything else is unresolved for the config's own reason.
    fn seat_without_config(step: &Step, task: &Task, input: &BTreeMap<String, String>, cause: &anyhow::Error) -> SeatClaim {
        match resolve_map_collection(step, task, input) {
            Ok(items) if items.is_empty() => SeatClaim::NoModel,
            Ok(_) => SeatClaim::LocalModelUnresolved { reason: format!("{cause:#}") },
            Err(e) => SeatClaim::LocalModelUnresolved { reason: format!("collection: {e:#}") },
        }
    }

    /// One per-item flow record, field-aligned with
    /// [`DispatchSingleShotStepKind`]'s hosted "step result" record so a
    /// graph/parity consumer reads a map's per-item records the same way it
    /// reads a single-shot's.
    fn item_record(session: &SessionId, execution: &ExecutionId, step: &Step, model: &str, unmanaged: bool, res: &MapItemResult) -> darkmux_flow::FlowRecord {
        darkmux_flow::FlowRecord {
            source: Some(darkmux_flow::FlowSource::Scheduler),
            model: Some(model.to_string()),
            ..darkmux_flow::FlowRecord::for_execution_with(
                session,
                execution,
                if res.ok { darkmux_flow::Level::Info } else { darkmux_flow::Level::Warn },
                darkmux_flow::Category::Work,
                darkmux_flow::Stage::Dispatch,
                darkmux_flow::Payload::StepResult(StepResultPayload {
                    index: Some(res.index as u64),
                    ok: Some(res.ok),
                    unmanaged: Some(unmanaged),
                    total_tokens: res.total_tokens,
                    // (#1442) Per-item telemetry: the endpoint-reported served
                    // model (HOSTED only; absent for a local item, by
                    // construction) and this item's cumulative dispatch wall-clock
                    // across every attempt.
                    served_model: res.served_model.clone(),
                    wall_ms: Some(res.wall_ms),
                    error: res.error.clone(),
                    ..StepResultPayload::new(&step.id, "dispatch.map")
                }),
                step.id.clone(),
            )
        }
    }

    /// The terminal of one item's execution (see [`ExecutionBookends`]):
    /// `dispatch.complete` for an item that produced a reply, `dispatch.error`
    /// for one that did not.
    fn item_terminal(records: &ExecutionBookends<'_>, res: &MapItemResult) -> darkmux_flow::FlowRecord {
        let tokens = crate::dispatch_envelope::DirectTokens {
            prompt_tokens: res.prompt_tokens,
            completion_tokens: res.completion_tokens,
            total_tokens: res.total_tokens,
            reasoning_tokens: res.reasoning_tokens,
            cached_tokens: res.cached_tokens,
        };
        // A reply is one turn with its own counts; an item that produced none took none.
        let base = if res.ok {
            crate::dispatch_internal::single_call_complete(&tokens, res.wall_ms, res.content.chars().count() as u64)
        } else {
            DispatchEndPayload { wall_ms: Some(res.wall_ms), ..DispatchEndPayload::new(0) }
        };
        let payload = DispatchEndPayload {
            item_index: Some(res.index as u64),
            result_class: Some(if res.ok { ResultClass::Ok } else { ResultClass::Error }),
            error: res.error.clone(),
            ..records.stamp(base)
        };
        if res.ok {
            records.record(darkmux_flow::Level::Info, darkmux_flow::Payload::DispatchComplete(payload))
        } else {
            records.record(darkmux_flow::Level::Error, darkmux_flow::Payload::DispatchError(payload))
        }
    }

    /// (#1442 gate C1) The ONE step-level aggregate record emitted after the
    /// whole loop: items_in, ok_count, failed_count, unmanaged, and SUMMED
    /// total_tokens across every item. See the emission site in `run` for
    /// why the sum (not the per-item values) is what the mission graph's
    /// max-fold token meter must see.
    fn aggregate_record(
        session: &SessionId,
        step: &Step,
        model: &str,
        unmanaged: bool,
        results: &[MapItemResult],
    ) -> darkmux_flow::FlowRecord {
        let ok_count = results.iter().filter(|r| r.ok).count();
        let failed_count = results.len() - ok_count;
        let total_tokens: u64 = results.iter().filter_map(|r| r.total_tokens).sum();
        // (#1442) Summed per-item dispatch wall-clock — trivially additive
        // beside the summed tokens, so the aggregate stays self-describing for
        // "how long did the whole map spend dispatching" without a consumer
        // re-folding the per-item records.
        let total_wall_ms: u64 = results.iter().map(|r| r.wall_ms).sum();
        darkmux_flow::FlowRecord {
            source: Some(darkmux_flow::FlowSource::Scheduler),
            model: Some(model.to_string()),
            ..darkmux_flow::FlowRecord::for_session_with(
                session,
                if failed_count == 0 { darkmux_flow::Level::Info } else { darkmux_flow::Level::Warn },
                darkmux_flow::Category::Work,
                darkmux_flow::Stage::Dispatch,
                darkmux_flow::Payload::StepResult(StepResultPayload {
                    items_in: Some(results.len() as u64),
                    ok_count: Some(ok_count as u64),
                    failed_count: Some(failed_count as u64),
                    unmanaged: Some(unmanaged),
                    total_tokens: Some(total_tokens),
                    total_wall_ms: Some(total_wall_ms),
                    ..StepResultPayload::new(&step.id, "dispatch.map")
                }),
                step.id.clone(),
            )
        }
    }

    /// The empty-collection short-circuit record (#1442): a NAMED reason so
    /// observability answers "why did this map not dispatch" directly.
    fn short_circuit_record(session: &SessionId, step: &Step) -> darkmux_flow::FlowRecord {
        darkmux_flow::FlowRecord {
            source: Some(darkmux_flow::FlowSource::Scheduler),
            model: load::<MapConfig>(step, ConfigKind::DispatchMap).ok().map(|cfg| cfg.call.model),
            ..darkmux_flow::FlowRecord::for_session_with(
                session,
                darkmux_flow::Level::Info,
                darkmux_flow::Category::Work,
                darkmux_flow::Stage::Dispatch,
                darkmux_flow::Payload::StepResult(StepResultPayload {
                    items_in: Some(0),
                    items_out: Some(0),
                    short_circuit: Some("empty collection: dispatch.map skipped before any model load".to_string()),
                    ..StepResultPayload::new(&step.id, "dispatch.map")
                }),
                step.id.clone(),
            )
        }
    }

    /// (#1442) The shared map body behind both the ctx-free [`StepKind::run`]
    /// and the streaming [`StepKind::run_streaming`]. `ctx` is `None` for the
    /// unit-test/no-scheduler path (records batch into
    /// `StepOutcome.flow_records`) and `Some` for the scheduler path
    /// (records emit LIVE).
    fn run_map(
        &self,
        step: &Step,
        task: &Task,
        input: &BTreeMap<String, String>,
        run_ctx: &StepRunCtx,
    ) -> Result<MapRun> {
        let ctx = run_ctx.live();
        let session = &run_ctx.session(self, step)?;
        let items = resolve_map_collection(step, task, input)?;
        let mut batched: Vec<darkmux_flow::FlowRecord> = Vec::new();
        // Emit LIVE through the scheduler's seam when a ctx is present
        // (#1442 gate C3, streaming); otherwise batch for return. `ctx` is
        // `Option<&_>` (Copy), so this closure captures it by copy — no
        // borrow conflict with the `&mut batched` it also takes per call.
        let push = |rec: darkmux_flow::FlowRecord, batched: &mut Vec<darkmux_flow::FlowRecord>| {
            match ctx {
                Some(c) => c.emit(rec),
                None => batched.push(rec),
            }
        };

        if items.is_empty() {
            // Runs BEFORE the `model`/`user_template` requirements — a
            // degenerate upstream that produced nothing is a clean completed
            // no-op, not a config error (mirrors the review verify seat's
            // empty-docket short-circuit). `residency` already returned
            // `None` for this input, so no model was loaded.
            push(Self::short_circuit_record(session, step), &mut batched);
            return Ok(MapRun {
                outcome: StepOutcome { output: "[]".to_string(), flow_records: batched, degraded: None },
                verdict: MapVerdict::Clean,
            });
        }

        let cfg: MapConfig = load_checked(step, ConfigKind::DispatchMap)?;
        let call = &cfg.call;
        // (#2570) The identifier this step actually ADDRESSES per item: the
        // bare `config.model` string for a hosted step (an endpoint
        // deployment name — darkmux never loads it into local residency),
        // or the SAME darkmux-namespaced identifier `seat()` derived above
        // for a local one. Pre-#2570 every per-item dispatch and every
        // record below used the bare `model` unconditionally, so a local
        // step with no `config.identifier` override loaded
        // `darkmux:<key>` and addressed `<key>` for every item — the
        // #2240/#2536 split, here. Computed once so the records and the
        // actual per-item calls can never disagree about what was
        // dispatched.
        let endpoint = step_endpoint(call).with_context(|| format!("step `{}`: config.endpoint", step.id))?;
        let managed_endpoint =
            step_managed_endpoint(call).with_context(|| format!("step `{}`: config.endpoint", step.id))?;
        let is_hosted = endpoint.is_some();
        let wire_model = step_wire_model(call, is_hosted);
        let user_template = cfg.user_template.as_str();
        let system = call.system.as_deref().unwrap_or("");
        let max_tokens = call.max_tokens.map_or(4096, |c| c.saturating_u32());
        let timeout_seconds = call.timeout_seconds.map_or(120, |c| c.saturating_u32());
        // (#1442) The retry budgets (default 0/off), read once for the whole
        // collection loop. A value beyond `u32`'s range was refused when the
        // config loaded (`MapConfig`'s rules): it must never become ~4 billion
        // re-dispatches. `retry_on_error` (#1605) retries a dispatch `Err`
        // instead of isolating it immediately; see [`DispatchMapStepKind`]'s
        // doc for the policy.
        let retry_on_empty = cfg.retry_on_empty.map_or(0, |n| n.saturating_u32());
        let retry_on_error = cfg.retry_on_error.map_or(0, |n| n.saturating_u32());

        // (#1607) Contract #2's liveness bookends are per ITEM (below): each
        // item is one role execution. They carry the endpoint label, which is
        // what gives a per-seat `task-<id>` session an endpoint to be
        // attributed by.
        let endpoint_label: Option<String> = endpoint
            .as_ref()
            .map(|ep| crate::target::endpoint_route_label(ep, wire_model.as_ref()));
        // (#2902 step 1a) The endpoint fact on each item's usage record: the
        // bookends' own label for a hosted step, else the LMStudio base the
        // local items call (`base_url: None` below, so the configured one).
        let usage_endpoint =
            endpoint_label.clone().unwrap_or_else(|| crate::usage::lmstudio_endpoint(None));
        // (#2344) Session-liveness heartbeat — the in-process twin of
        // `dispatch_internal`'s container-path emitter (#638). `dispatch.map`
        // performs REAL model work in the per-item loop below (`single_shot_chat`
        // / the hosted single-shot call), but unlike the container path it
        // never wrote a `darkmux:session-presence:<sid>` beat, so a
        // long-running map (a probe/judge/verify seat with many items, or one
        // hosted item taking real wall-clock) never showed up in the live
        // fleet view while GENERATING — only its `dispatch start`/`complete`
        // bookends (#1607, above) landed, and those are terminal-only, not a
        // liveness signal. Self-disables when `DARKMUX_REDIS_URL` is unset,
        // same gate the bookend's flow sink uses. Uses the SAME session id
        // every record on this path uses (the step's task session), so a beat and
        // its bookends key on the identical session. Stopped explicitly right
        // after the loop (below) on the clean path; `SessionEmitter::drop`
        // (#2344) is the backstop for a `?`/panic in between — same
        // discipline `dispatch_internal`'s `session_emitter` uses, and it now
        // removes the key itself (pre-claim + DEL), the same teardown
        // `stop()` runs, rather than only halting the refresh thread and
        // leaving the TTL to age the beat out.
        //
        // WHY TASK-SCOPED, not step-scoped (fresh-review finding). The key
        // has to be the one the RECORDS use, because the live view joins the
        // beat to the session the bookends and per-item records name — and
        // #1979 pins both this kind and `dispatch.single_shot` to
        // the task session deliberately (sibling seats fanned out within
        // one task share a join key so a seat's tokens tie to its endpoint;
        // only `dispatch.internal`, a solo dispatch, is step-scoped). A beat
        // keyed on the step would be presence for a session no record names.
        //
        // The known cost, named rather than left to be rediscovered: ANY
        // teardown that reaches `remove_presence_key` — a clean `stop()`, or
        // (#2344) `SessionEmitter::drop` catching an early return or a caught
        // panic — pre-claims `edge-claim:session-end:<task-sid>` for
        // `EDGE_CLAIM_TTL_SECS` (60s), so a LATER model-bearing step in the
        // SAME task that is abandoned inside that window loses the claim and
        // the presence reconciler skips its `session.end` edge — playback
        // then lacks the close bracket for exactly the abandonment case that
        // edge exists to provide. Steps are linear within a task and no
        // shipped mission config puts two model-bearing steps in one task,
        // so this is a sequencing hazard, not a live race. If one ever does,
        // the fix is at the session convention (rekey the WHOLE vocabulary
        // together), never at the beat alone.
        let session_emitter = darkmux_flow::session_presence::spawn_session_emitter(
            session,
            task.role_id.clone(),
            Some(wire_model.to_string()),
        );

        // (#1442 ship-2b) The scheduler-supplied dispatch override, if any —
        // threaded into every item's arm; `None` on all production paths.
        let ovr = run_ctx.dispatch_override();

        let temperature = call.temperature();
        let mut results: Vec<MapItemResult> = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            // One item, one execution: its bookends, its usage records and
            // the budget records of its gate all name it.
            let execution = &ExecutionId::mint();
            let records = ExecutionBookends {
                kind: "dispatch.map",
                session,
                execution,
                step,
                model: wire_model.as_ref(),
                endpoint_label: endpoint_label.as_deref(),
            };
            let mut bookend = records.open(
                ctx,
                Some(index as u64),
                "dispatch.map item terminated before completion (early return or panic)",
            );
            let budget_caller = crate::budget::BudgetCaller {
                session,
                execution,
                role_id: task.role_id.as_deref(),
                model: Some(wire_model.as_ref()),
                phase_id: Some(&task.phase_id),
                profiles_file: call.config_path.as_deref(),
            };
            // (#2310 P1 review finding I1) A `{system, item}` override wins
            // for THIS item's dispatch; every other item shape keeps using
            // the step's own `config.system` unchanged — see
            // `item_system_and_payload`'s own doc.
            let (item_system_override, payload) = item_system_and_payload(item);
            let item_system = item_system_override.unwrap_or(system);
            let user = user_template.replace("{item}", &map_item_text(payload));
            let mut calls: Vec<MapCall> = Vec::new();
            let res = match &endpoint {
                Some(ep) => {
                    // One item is one dispatch: its own token cap bucket.
                    let item_bucket =
                        Mutex::new(DispatchBudget::for_endpoint(ep).map_err(|e| anyhow::anyhow!(e))?);
                    map_hosted_item(
                        index, &item_bucket, ep, wire_model.as_ref(), item_system, &user, max_tokens,
                        timeout_seconds, retry_on_empty, retry_on_error, ovr, &mut calls,
                        &step.id, &budget_caller,
                    )
                }
                None => {
                    // (#3035) A managed `config.endpoint` carries its limits:
                    // one item is one dispatch with its own cap bucket.
                    let item_bucket = item_cap_bucket(managed_endpoint.as_ref())?;
                    let limits = LocalLimits::of(managed_endpoint.as_ref(), item_bucket.as_ref(), &step.id, &budget_caller);
                    map_local_item(
                        index, wire_model.as_ref(), item_system, &user, temperature, max_tokens,
                        timeout_seconds, retry_on_empty, retry_on_error, ovr, &mut calls, limits.as_ref(),
                    )
                }
            };
            // (#1442 gate C3) LIVE per-item emission when streaming.
            push(Self::item_record(session, execution, step, wire_model.as_ref(), endpoint.is_some(), &res), &mut batched);
            // (#1442 ship-2b, #1361 continuity) `telemetry.tokens` records
            // for this item's calls (see #2902 below), so the fleet
            // dashboard's off-meter token sum (`category: telemetry,
            // source: tokens` records ONLY) stays sighted on map-dispatched
            // work — the review pipeline's probe/verify stages ride this
            // block now, and their per-call telemetry emission retired with
            // their bespoke kinds.
            //
            // (#1530 dogfood) The prompt/completion SPLIT rides along now
            // that [`MapItemResult`] carries it. It is not decoration: the
            // dashboard's `tokensOffMeter()` sums `total_tokens` into the
            // headline but classifies via the split, so emitting the total
            // alone made every map-dispatched call headline-visible and
            // chip-invisible — 41% of the 2.3.0 dogfood's local tokens
            // landed in no bucket. Still never FABRICATED: a provider that
            // reported no split leaves both fields `None`, and the payload
            // omits them entirely rather than claiming a zero.
            //
            // (#2902 step 1a) One record per model CALL: an item that retried
            // made several, and each one's own counts and reported model are
            // accounted separately (they sum to the item's totals above). An
            // attempt that replied without usage emits `absent`; an attempt
            // with no reply made no completed call and emits nothing.
            for call in &calls {
                push(
                    crate::dispatch::build_telemetry_record(
                        darkmux_flow::Level::Info,
                        darkmux_flow::FlowSource::Tokens,
                        &step.id,
                        session,
                        execution,
                        Some(wire_model.as_ref()),
                        None,
                        darkmux_flow::Payload::TelemetryTokens(map_call_token_payload(
                            call,
                            res.index,
                            wire_model.as_ref(),
                            &usage_endpoint,
                            endpoint.as_ref().or(managed_endpoint.as_ref()).and_then(|ep| ep.named_id()),
                        )),
                    ),
                    &mut batched,
                );
            }
            // (#1607) The item's terminal, after its usage records: `ok` is
            // a completed execution, anything else an errored one.
            bookend.close(Self::item_terminal(&records, &res));
            results.push(res);
        }

        // (#2344) The per-item loop is done — no more model work is in
        // flight for this step — so stop the heartbeat here, before the
        // terminal records below, mirroring `dispatch_internal`'s "the
        // container has exited — the session is no longer running" comment
        // at its own stop site. `stop()` joins the beat thread and DELetes
        // the key for an instant drop on the live view instead of waiting
        // out the TTL; a `?`/panic earlier in this function never reaches
        // here, but `SessionEmitter::drop` (#2344) removes the key itself
        // the same way on the way out, rather than only relying on the TTL.
        if let Some(em) = session_emitter {
            em.stop();
        }

        // (#1442 gate C1) ONE step-level aggregate record after the loop.
        // Load-bearing: the mission graph's token meter folds "step result"
        // token fields per step with Math.max (a one-record-per-step
        // assumption) — with only N per-item records, a map step's meter
        // would render as the LARGEST single item, not the step's spend. The
        // aggregate's SUMMED total_tokens is >= every per-item value, so the
        // existing max-fold reads the true spend with zero viewer changes.
        push(Self::aggregate_record(session, step, wire_model.as_ref(), endpoint.is_some(), &results), &mut batched);

        let output = serde_json::to_string(&results).context("serializing dispatch.map results")?;
        Ok(MapRun {
            outcome: StepOutcome { output, flow_records: batched, degraded: None },
            verdict: MapVerdict::of(&results),
        })
    }
}

/// How a `dispatch.map` step's items came out, decided once from its
/// results: every item failing is the step failing (nothing usable came out),
/// some failing is a degraded step (its output is real but incomplete), none
/// failing is a clean step.
#[derive(Debug, PartialEq, Eq)]
enum MapVerdict {
    AllFailed { total: usize, first_error: String },
    Partial { failed: usize, total: usize },
    Clean,
}

/// What one pass over a map's items produced: the step's outcome as if every
/// item had been fine, and how the items actually came out.
struct MapRun {
    outcome: StepOutcome,
    verdict: MapVerdict,
}

impl MapRun {
    /// The step's real outcome: an error when every item failed, the outcome
    /// marked degraded when some did, unchanged when none did.
    fn into_outcome(self, step_id: &str) -> Result<StepOutcome> {
        match self.verdict {
            MapVerdict::AllFailed { total, first_error } => Err(anyhow!(
                "step `{step_id}` dispatch.map: all {total} item(s) failed; first error: {first_error}"
            )),
            MapVerdict::Partial { failed, total } => Ok(StepOutcome {
                degraded: Some(format!("dispatch.map step `{step_id}`: {failed} of {total} item(s) failed")),
                ..self.outcome
            }),
            MapVerdict::Clean => Ok(self.outcome),
        }
    }
}

impl MapVerdict {
    fn of(results: &[MapItemResult]) -> Self {
        let failed = results.iter().filter(|r| !r.ok).count();
        let total = results.len();
        match failed {
            0 => MapVerdict::Clean,
            n if n == total => MapVerdict::AllFailed {
                total,
                first_error: results.iter().find_map(|r| r.error.clone()).unwrap_or_else(|| "no error recorded".to_string()),
            },
            failed => MapVerdict::Partial { failed, total },
        }
    }
}

/// (#1607) RAII half of a step kind's liveness bookends — shared by
/// `dispatch.map` and (#2344) `dispatch.single_shot`, which owes contract #2
/// the same pair for the same reason. Named for the STEP rather than for
/// either kind: nothing in it knows what a map item or a single shot is.
///
/// Contract #2 requires a terminal on ALL exit paths, and `run_map` has many:
/// a `?` on collection resolution, on the budget admit gate, on serialization,
/// plus a panic in the loop. A hand-written "emit the terminal before each
/// return" would be correct exactly until the next `?` is added — which is how
/// #1272 happened the first time. The Drop impl makes forgetting impossible.
///
/// `close` consumes the pre-built error terminal and emits the caller's real
/// one instead, so exactly ONE terminal is emitted per open — never two.
///
/// Streaming vs batched: with a `ctx` the records go out live, so a terminal
/// emitted from Drop during unwinding still reaches the sink. WITHOUT a ctx
/// (the `run()` path, tests) records are returned in `StepOutcome` and are
/// already lost on `Err`, so Drop has nowhere useful to put one — it no-ops
/// rather than pretending. Production always has a ctx (`run_streaming`).
struct StepBookend<'a> {
    ctx: Option<&'a StepRunCtx>,
    /// The terminal to emit if dropped without `close` — taken by `close`.
    on_abort: Option<darkmux_flow::FlowRecord>,
}

impl<'a> StepBookend<'a> {
    fn new(
        ctx: Option<&'a StepRunCtx>,
        start: darkmux_flow::FlowRecord,
        on_abort: darkmux_flow::FlowRecord,
    ) -> Self {
        if let Some(c) = ctx {
            c.emit(start);
        }
        Self { ctx, on_abort: Some(on_abort) }
    }

    /// Emit the real terminal and disarm.
    ///
    /// Without a ctx this emits NOTHING — deliberately. `new` can only emit
    /// the START through a ctx, so pushing a terminal into the batched vec
    /// here would produce an ORPHAN: a `dispatch complete` with no matching
    /// `dispatch start`. Liveness surfaces key on the PAIR (#857), so half a
    /// pair is worse than neither — it reads as a dispatch that ended without
    /// ever beginning. The guard is therefore entirely inert without a
    /// streaming sink, and the batched `flow_records` shape is unchanged from
    /// before this bookend existed.
    ///
    /// (Found by the workspace suite: pushing here appended a record that
    /// shifted `flow_records.last()` off the aggregate and broke two existing
    /// tests. They were right and the push was wrong.)
    fn close(&mut self, finished: darkmux_flow::FlowRecord) {
        self.on_abort = None;
        if let Some(c) = self.ctx {
            c.emit(finished);
        }
    }

    /// (#3121) Emit the abort terminal naming `error`, the failure that ended
    /// the execution, and how long it ran, then disarm. The Drop fallback's
    /// generic text is for a panic or an unexplained early return only.
    fn fail(&mut self, error: &anyhow::Error, wall_ms: u64) {
        if let Some(mut rec) = self.on_abort.take() {
            if let Some(darkmux_flow::Payload::DispatchError(p)) = rec.payload.as_mut() {
                p.error = Some(format!("{error:#}"));
                p.wall_ms = Some(wall_ms);
            }
            self.on_abort = Some(rec);
        }
        self.emit_abort();
    }

    /// Emit the armed abort terminal, stamped now: it was built when the
    /// guard opened, and a terminal carrying its start time reads as a
    /// zero-length execution (#3121).
    fn emit_abort(&mut self) {
        if let (Some(mut rec), Some(c)) = (self.on_abort.take(), self.ctx) {
            rec.ts = darkmux_flow::ts_utc_now();
            // Armed when the execution started: name a stop that came since.
            c.emit(rec.naming_operator_stop());
        }
    }
}

impl Drop for StepBookend<'_> {
    fn drop(&mut self) {
        self.emit_abort();
    }
}

// (#1442 gate — dispatch.map hosted seam) The hosted dispatch primitive
// `dispatch.map`'s hosted arm calls, with a `#[cfg(test)]` injection point
// that MIRRORS `review.rs`'s `chat_override` field: production has NO seam
// (the whole hook is compiled out), exactly as review's field is `None` in
// production. `dispatch.map` is a process-wide unit-struct builtin (no
// per-instance context to hang a field off), so the test seam is a
// thread-local rather than a struct field — the idiomatic equivalent for a
// builtin. Unit tests that exercise the hosted collection loop (partial
// mid-collection exhaustion, honest-`None` token accounting) install a
// closure here and drive `run`/`run_map` on the SAME thread.
#[cfg(test)]
thread_local! {
    #[allow(clippy::type_complexity)]
    static MAP_HOSTED_OVERRIDE: std::cell::RefCell<
        Option<Box<dyn Fn(&crate::single_shot::HostedSingleShotRequest) -> Result<crate::single_shot::SingleShotReply>>>,
    > = const { std::cell::RefCell::new(None) };
}

fn map_hosted_dispatch(
    req: &crate::single_shot::HostedSingleShotRequest,
) -> Result<crate::single_shot::SingleShotReply> {
    #[cfg(test)]
    {
        let hooked = MAP_HOSTED_OVERRIDE.with(|o| o.borrow().is_some());
        if hooked {
            return MAP_HOSTED_OVERRIDE.with(|o| (o.borrow().as_ref().unwrap())(req));
        }
    }
    crate::single_shot::single_shot_chat_hosted(req)
}

impl MapItemResult {
    /// An item's token fields: the sum of its calls' counts, the SAME counts
    /// each call's usage record carries (one record per attempt), so the
    /// item and its records cannot disagree. A field no call reported stays
    /// `None`, never a fabricated 0; `total_tokens` sums each call's
    /// [`darkmux_trajectory::UsageCounts::total_tokens`]. The other fields
    /// are left default for the caller's struct-update.
    fn tokens_of(calls: &[MapCall]) -> Self {
        let sum = |f: fn(&darkmux_trajectory::UsageCounts) -> Option<u64>| {
            calls.iter().filter_map(|c| f(&c.counts)).reduce(u64::saturating_add)
        };
        Self {
            total_tokens: sum(darkmux_trajectory::UsageCounts::total_tokens),
            prompt_tokens: sum(|c| c.prompt),
            completion_tokens: sum(|c| c.completion),
            reasoning_tokens: sum(|c| c.reasoning),
            cached_tokens: sum(|c| c.cached),
            ..Self::default()
        }
    }
}

/// (#2902 step 1a) What one `dispatch.map` model call reported, captured the
/// moment its reply returns — one per ATTEMPT, so an item that retries
/// (`retry_on_empty` / `retry_on_error`) accounts every call it made, and an
/// attempt that replied with no usage is still accounted (`absent`) even when
/// a later attempt errors. An attempt that got no reply (a transport error,
/// a budget skip) made no completed call and pushes nothing.
#[derive(Debug, Clone)]
pub(crate) struct MapCall {
    pub counts: darkmux_trajectory::UsageCounts,
    /// The response's own `model` field, local and hosted alike.
    pub reported_model: Option<String>,
}

impl MapCall {
    fn from_reply(reply: &crate::single_shot::SingleShotReply) -> Self {
        Self { counts: reply.counts.clone(), reported_model: reply.model.clone() }
    }
}

/// (#1530 dogfood, #2902 step 1a) Pure: one map model CALL's
/// `telemetry.tokens` payload, through the one usage writer. Every call that
/// got a reply emits one (`token_source: "absent"` when it carried no usage);
/// see [`MapCall`]. Split out from the emitter so the payload SHAPE
/// is unit-testable without a flow sink, the same pure-payload/emitter
/// division `turn_tokens_payload` and `review_token_telemetry_payload` use.
///
/// The split fields are omitted, never zeroed, when the provider didn't
/// report them. NOTE the sibling emitter `review_token_telemetry_payload`
/// (`darkmux-lab`'s `review.rs`) takes the OPPOSITE approach and fabricates
/// a split from the total; both feed the same `telemetry.tokens` family, and
/// reconciling them onto one policy is deliberate follow-up, not an
/// oversight — do not "fix" one to match the other without deciding which is
/// right for both.
///
/// `total_tokens` falls back to `prompt + completion` when the provider
/// reported a split but no total (`single_shot.rs` reads all three fields
/// independently, so that combination is representable). That is ARITHMETIC
/// on reported numbers, not fabrication, and it matches what the viewer's
/// own `tokensOffMeter()` already does for the single-shot hosted path.
/// Without it, a split the item genuinely had would be dropped entirely —
/// the exact class of loss this function exists to close.
///
/// (#2690, FLOW_SCHEMA_VERSION 1.49.0) `unmanaged` and `index` are the SEAT's
/// own identity, and they are here because the viewer could not otherwise
/// recover it. `session_id` on this record is the step's task session
/// (`SessionId::task(run, &step.task_id)`), which sibling seats fanned out within ONE task SHARE
/// by construction — and `mission_id` is shared too, so the savings hero's
/// `(session_id, mission_id)` run key cannot separate them either. The hero
/// therefore fell back to a per-KEY rule: if ANY bookend under the key named
/// a hosted endpoint, every token arriving under it counted CLOUD. That
/// over-claims cloud for a genuinely mixed task — it reports the operator's
/// OWN HARDWARE's work as hosted spend, which is the defect #2690 is about,
/// measured at three arities (`ui/src/lenses/fleet/savings.test.ts`).
///
/// This record now carries the answer instead of the consumer guessing it.
/// `unmanaged` is the step's own unmanaged-or-managed verdict — the SAME
/// `endpoint.is_some()` this kind already stamps on its `step result`
/// (`Self::item_record`) and aggregate records, so a seat's telemetry and a
/// seat's per-item record cannot disagree. It is uniform across a step's
/// items by construction: `run_map` resolves ONE `endpoint` for the whole
/// step and every item takes the same arm (`map_hosted_item` vs
/// `map_local_item`).
///
/// `index` rides along as the item's own position, which is what makes two
/// telemetry records of the SAME step distinguishable at all — the fields
/// this payload otherwise carries are all counts, and two items can
/// legitimately report identical ones.
///
/// WHY THE PRODUCER AND NOT THE VIEWER. `Self::item_record` already emits a
/// literal per-seat `unmanaged` for the same item, pushed from the same
/// `MapItemResult` in the same loop iteration — so a consumer COULD join the
/// two. Measured on the committed parity corpora, that join is
/// `(session_id, ts, total_tokens)`, it pairs 180 of 364 telemetry records,
/// and its collision case (k identical draws of one prompt closing inside
/// one second — the normal shape of a probe stage) is exactly the shape the
/// map fan-out produces. A field costs one key and cannot collide.
///
/// SCOPE, stated because "half the population" was the reason this was
/// declined twice. That figure measured the JOIN's reach across EVERY
/// `telemetry.tokens` record. The DEFECT's population is narrower: only a
/// record under a seat-SHARING session id can be misattributed, and
/// the task session is used by exactly two step kinds
/// (`dispatch.single_shot` and `dispatch.map`). (#2902 step 1a: the
/// single-shot kind now emits `telemetry.tokens` too, `call_kind:
/// "single_shot"`; the hero still counts that kind from its own `dispatch
/// complete` bookend, classified per completion, and skips its usage
/// records, `ui/src/lib/usageRecords.ts`.) The other live producer, the container path's per-turn tailer
/// (`dispatch_internal.rs`'s `emit_telemetry`), runs under
/// `session_id::step(&step.id)` (`dispatch_opts_for`, this file) — unique per
/// step — and `dispatch.unit` mints `crawl-<mission>-<rule>-<unit>` plus a
/// per-draw suffix. Neither can share a key with another seat. So this one
/// emitter is the whole live population.
fn map_call_token_payload(
    call: &MapCall,
    index: usize,
    requested_model: &str,
    endpoint: &str,
    endpoint_id: Option<&str>,
) -> crate::usage::UsagePayload {
    let mut payload = crate::usage::usage_payload(
        &crate::usage::CallFacts {
            call_kind: crate::usage::CallKind::MapItem,
            role_id: None,
            requested_model,
            reported_model: call.reported_model.as_deref(),
            endpoint,
            endpoint_id,
        },
        &call.counts,
    );
    // Unconditional, unlike every count: a fact about THIS emitter's own call,
    // never something a provider did or did not report. `index` is the ITEM's
    // position; an item that retried emits one record per attempt, all
    // carrying the same index. (The record carries no hosted/local flag:
    // darkmux reports the endpoint and model it called, never a topology.)
    payload.index = Some(index as u64);
    payload
}

/// (tests) The payload one call with `res`'s counts would carry — the
/// payload-shape tests below predate per-attempt records and read an item's
/// accumulated counts as if they were one call's.
#[cfg(test)]
fn map_item_token_payload(
    res: &MapItemResult,
    requested_model: &str,
    endpoint: &str,
) -> Option<crate::usage::UsagePayload> {
    let call = MapCall {
        counts: darkmux_trajectory::UsageCounts {
            prompt: res.prompt_tokens,
            completion: res.completion_tokens,
            total: res.total_tokens,
            reasoning: res.reasoning_tokens,
            cached: res.cached_tokens,
            cache_write: None,
        },
        reported_model: res.served_model.clone(),
    };
    Some(map_call_token_payload(&call, res.index, requested_model, endpoint, None))
}

/// (#1605) The bounded transient-error retry's backoff — short on purpose
/// (this is a per-item pause inside an already-bounded dispatch, not a
/// rate-limit ladder; `single_shot_chat`/`single_shot_chat_hosted` already
/// own a real 429/503 backoff ladder underneath this, per
/// [`DispatchMapStepKind`]'s `retry_on_error` doc). Only ever slept when
/// `retry_on_error > 0` AND an attempt actually errored — the overwhelming
/// majority of items (every default-config caller, and every successful
/// dispatch) never pay it.
const RETRY_ON_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_millis(200);

/// (#1442) One LOCAL map item — dispatch, with the generic `retry_on_empty`
/// loop (default 0/off) and the `retry_on_error` loop (#1605, default
/// 0/off). Tokens accumulate across every attempt; the loop stops early on
/// the first non-empty reply. See [`DispatchMapStepKind`]'s doc for the
/// full `retry_on_empty`/`retry_on_error` semantics.
#[allow(clippy::too_many_arguments)]
fn map_local_item(
    index: usize,
    model: &str,
    system: &str,
    user: &str,
    temperature: f32,
    max_tokens: u32,
    timeout_seconds: u32,
    retry_on_empty: u32,
    retry_on_error: u32,
    ovr: Option<&MapDispatchOverride>,
    calls: &mut Vec<MapCall>,
    limits: Option<&LocalLimits<'_>>,
) -> MapItemResult {
    use crate::single_shot::{single_shot_chat, SingleShotRequest};
    // (#1442) Cumulative dispatch wall-clock across every attempt, as the
    // tokens are (`MapItemResult::tokens_of` over `calls`). A LOCAL item's
    // `served_model` is ALWAYS `None` by construction (see [`MapItemResult`]'s
    // doc): the response body's echoed `model` is not ground truth for a local
    // dispatch, so this arm never reads it.
    let mut wall_ms = 0u64;
    let mut empty_budget = retry_on_empty;
    let mut error_budget = retry_on_error;
    let mut error_retries_used = 0u32;
    let mut last_error: Option<String> = None;
    loop {
        // (#3035) A managed endpoint's window gate, then this item's dispatch
        // cap reservation, before every attempt. Only a run stopped during a
        // wait (or limits that cannot be used) ends the item, with nothing sent.
        if let Some(l) = limits {
            if let Err(e) = crate::budget::admit_endpoint(l.endpoint, l.caller) {
                return MapItemResult {
                    index,
                    ok: false,
                    content: String::new(),
                    error: Some(format!("{e:#}")),
                    served_model: None,
                    wall_ms,
                    retried: error_retries_used,
                    ..MapItemResult::tokens_of(calls)
                };
            }
        }
        let req = SingleShotRequest {
            base_url: None,
            model,
            system,
            user,
            temperature,
            max_tokens,
            timeout_seconds,
        };
        let t0 = std::time::Instant::now();
        // (#1442 ship-2b) The scheduler-supplied override replaces the
        // TRANSPORT only — retry semantics and token accounting are
        // identical on both paths (see [`MapDispatchOverride`]).
        let dispatch = match ovr {
            Some(f) => f(&OverrideDispatchCall {
                model,
                system,
                user,
                temperature,
                max_tokens,
                timeout_seconds,
                endpoint: None,
            }),
            None => single_shot_chat(&req),
        };
        wall_ms += t0.elapsed().as_millis() as u64;
        if let Some(l) = limits {
            let spent = dispatch.as_ref().map_or(0, |r| {
                crate::budget::conservative_spend(r.counts.total_tokens(), max_tokens, &format!("{system}{user}"))
            });
            crate::budget::settle_dispatch_live(l.bucket, spent, l.label, l.caller);
        }
        match dispatch {
            Ok(reply) => {
                calls.push(MapCall::from_reply(&reply));
                if !reply.content.trim().is_empty() {
                    return MapItemResult {
                        index,
                        ok: true,
                        content: reply.content,
                        error: None,
                        served_model: None,
                        wall_ms,
                        retried: error_retries_used,
                        ..MapItemResult::tokens_of(calls)
                    };
                }
                // Empty content — retry (until the budget is spent).
                if empty_budget == 0 {
                    break;
                }
                empty_budget -= 1;
            }
            // (#1605) A dispatch `Err` retries ONLY when `retry_on_error`
            // opted in (default 0 — the historical "never retried, a
            // second-guessing retry here would hide a real infra problem"
            // behavior stays the default for every caller that doesn't ask
            // for otherwise). When it does retry, a short backoff separates
            // the attempts and `error_retries_used` tracks how many fired,
            // so a recovered item is distinguishable from one that never
            // needed to retry.
            Err(e) => {
                if error_budget == 0 {
                    // (Also fix, #2570 class) `model` here is always the
                    // LOCAL wire id `run_map` resolved before this loop —
                    // see `with_residency_lost_hint`'s own doc for why this
                    // needs the same explanation `dispatch_internal`'s
                    // top-level local dispatch already attaches.
                    let e = with_residency_lost_hint(model, e);
                    return MapItemResult {
                        index,
                        ok: false,
                        content: String::new(),
                        error: Some(format!("{e:#}")),
                        served_model: None,
                        wall_ms,
                        retried: error_retries_used,
                        ..MapItemResult::tokens_of(calls)
                    };
                }
                error_budget -= 1;
                error_retries_used += 1;
                last_error = Some(format!("{e:#}"));
                std::thread::sleep(RETRY_ON_ERROR_BACKOFF);
            }
        }
    }
    // (#3074) Reaching here means no attempt produced content. When no
    // attempt errored, every one came back empty: the item DISPATCHED (ok),
    // produced no usable content, and its whole spend is billed (the
    // reasoning-guillotine case the probe stage's retry loop already
    // handled). When an earlier attempt errored and the retry came back
    // empty, `ok` is a claim about what happened, so the item reports that
    // error instead of an empty success (the hosted sibling's rule, #1605).
    MapItemResult {
        index,
        ok: last_error.is_none(),
        content: String::new(),
        error: last_error,
        served_model: None,
        wall_ms,
        retried: error_retries_used,
        ..MapItemResult::tokens_of(calls)
    }
}

/// (#1442) One HOSTED map item: the budgeted sibling of [`map_local_item`].
/// Each attempt (including a `retry_on_empty` or `retry_on_error`, #1605,
/// retry) first passes the endpoint's rolling-window budget and the item's
/// own token cap bucket (#3035: `crate::budget`), then settles its real cost
/// against the cap after the call. No attempt is ever skipped or clamped for
/// budget: a breach warns, and an endpoint `wait` holds the attempt until
/// there is room. Only a run stopped during a wait (Ctrl-C, `mission abort`)
/// ends the item early, reported as its error, with nothing sent.
#[allow(clippy::too_many_arguments)]
fn map_hosted_item(
    index: usize,
    bucket: &Mutex<DispatchBudget>,
    endpoint: &darkmux_types::ModelEndpoint,
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    timeout_seconds: u32,
    retry_on_empty: u32,
    retry_on_error: u32,
    ovr: Option<&MapDispatchOverride>,
    calls: &mut Vec<MapCall>,
    dispatch_label: &str,
    caller: &crate::budget::BudgetCaller<'_>,
) -> MapItemResult {
    use crate::single_shot::HostedSingleShotRequest;
    // (#1442) Cumulative dispatch wall-clock across every attempt, and the
    // ENDPOINT-reported served model — captured from
    // the reply body's `model` field (last non-`None` across attempts wins, so
    // a later usage-less reply never erases a served model an earlier attempt
    // reported).
    let mut wall_ms = 0u64;
    let mut served_model: Option<String> = None;
    let mut empty_budget = retry_on_empty;
    let mut error_budget = retry_on_error;
    // (#1605 QA finding) The error that sent us into a retry, kept so the
    // fallthrough below cannot report success for an item whose only real
    // dispatch FAILED. Before the retry arm existed, `Err` returned
    // immediately and the fallthrough's `ok: true` was sound — the only way
    // to reach it was a dispatched-but-empty reply. Retrying widened the set
    // of states that reach it without re-deriving that honesty.
    let mut last_error: Option<String> = None;
    let mut error_retries_used = 0u32;
    let clamped = max_tokens;
    let req = HostedSingleShotRequest {
        endpoint,
        model,
        system,
        user,
        max_tokens: clamped,
        timeout_seconds,
    };
    // The body every attempt posts: an unreported spend is charged against
    // the prompt it carries. A dialect that cannot be resolved fails the
    // item here, before any budget is touched, as the send itself would.
    let body = match req.body() {
        Ok(b) => b,
        Err(e) => {
            return MapItemResult {
                index,
                ok: false,
                content: String::new(),
                error: Some(format!("{e:#}")),
                served_model,
                wall_ms,
                retried: 0,
                ..MapItemResult::tokens_of(calls)
            };
        }
    };
    loop {
        // (#2902 step 5, #3035) The endpoint's window gate. It may warn or,
        // under `wait`, hold; only a run stopped during a wait (or limits
        // that cannot be used) returns here, and then nothing is sent.
        if let Err(e) = crate::budget::admit_endpoint(endpoint, caller) {
            return MapItemResult {
                index,
                ok: false,
                content: String::new(),
                error: Some(format!("{e:#}")),
                served_model,
                wall_ms,
                retried: error_retries_used,
                ..MapItemResult::tokens_of(calls)
            };
        }
        let t0 = std::time::Instant::now();
        // (#1442 ship-2b) Scheduler-supplied override replaces the TRANSPORT
        // only — the reserve/settle metering around it is identical.
        let dispatch = match ovr {
            Some(f) => f(&OverrideDispatchCall {
                model,
                system,
                user,
                temperature: 0.0,
                max_tokens: clamped,
                timeout_seconds,
                endpoint: Some(endpoint),
            }),
            None => map_hosted_dispatch(&req),
        };
        wall_ms += t0.elapsed().as_millis() as u64;
        match dispatch {
            Ok(reply) => {
                calls.push(MapCall::from_reply(&reply));
                crate::budget::settle_dispatch_live(
                    bucket,
                    crate::budget::conservative_hosted_spend(reply.counts.total_tokens(), clamped, &body),
                    dispatch_label,
                    caller,
                );
                if reply.model.is_some() {
                    served_model = reply.model.clone();
                }
                if !reply.content.trim().is_empty() {
                    return MapItemResult {
                        index,
                        ok: true,
                        content: reply.content,
                        error: None,
                        served_model,
                        wall_ms,
                        retried: error_retries_used,
                        ..MapItemResult::tokens_of(calls)
                    };
                }
                // Empty content: retry, when `retry_on_empty` allows.
                if empty_budget == 0 {
                    break;
                }
                empty_budget -= 1;
            }
            // (#1605) See `map_local_item`'s matching arm — same policy:
            // retried only when `retry_on_error` opted in, with a short
            // backoff and `error_retries_used` tracking how many fired.
            Err(e) => {
                // The one hosted-error policy (`call_may_have_spent`): a call
                // the endpoint may have processed is charged like a reply
                // with no usage, and counted as a call; any other error
                // spends nothing.
                if crate::dispatch_internal::call_may_have_spent(&e) {
                    calls.push(MapCall { counts: Default::default(), reported_model: None });
                }
                crate::budget::settle_dispatch_live(
                    bucket,
                    crate::dispatch_internal::unanswered_hosted_spend(&e, clamped, &body),
                    dispatch_label,
                    caller,
                );
                if error_budget == 0 {
                    return MapItemResult {
                        index,
                        ok: false,
                        content: String::new(),
                        error: Some(format!("{e:#}")),
                        served_model,
                        wall_ms,
                        retried: error_retries_used,
                        ..MapItemResult::tokens_of(calls)
                    };
                }
                error_budget -= 1;
                error_retries_used += 1;
                last_error = Some(format!("{e:#}"));
                std::thread::sleep(RETRY_ON_ERROR_BACKOFF);
            }
        }
    }
    // (#1605 QA finding) `ok` is a claim about what actually happened. An
    // item that reaches here after an errored attempt (an error retry that
    // came back empty) must report that error, not an empty success.
    MapItemResult {
        index,
        ok: last_error.is_none(),
        content: String::new(),
        error: last_error,
        served_model,
        wall_ms,
        retried: error_retries_used,
        ..MapItemResult::tokens_of(calls)
    }
}

impl StepKind for DispatchMapStepKind {
    fn id(&self) -> &'static str {
        ConfigKind::DispatchMap.id()
    }

    fn display_name(&self) -> &'static str {
        "Dispatch (map)"
    }

    /// (#1979) Task-scoped, NOT the trait default's step scope. Deliberate:
    /// sibling seats fanned out within one task share this key so a
    /// consumer can join a seat's tokens to its endpoint (see the record
    /// built in this kind's own dispatch path). The step remains individually attributable through
    /// `payload.step_id` and `handle` — grouping and identity are different
    /// jobs, and this field is the grouping one.
    fn session_scope(&self) -> SessionScope {
        SessionScope::Task
    }


    /// (#1442 gate C3) LIVE per-item emission through the [`StepRunCtx`]
    /// channel when the scheduler supplies one (so a 30-item map lands items
    /// on the graph page as they finish, never batched at wave-drain). A
    /// context with no emitter (a step run on its own) batches every record
    /// into `StepOutcome.flow_records`.
    fn run(&self, step: &Step, task: &Task, input: &BTreeMap<String, String>, ctx: &StepRunCtx) -> Result<StepOutcome> {
        self.run_map(step, task, input, ctx)?.into_outcome(&step.id)
    }

    /// (#1442, restated as a seat claim by #2394) Four genuinely different
    /// answers this kind can give, which the old `Option<Placement>` had to
    /// squeeze into one `None`:
    ///
    /// - `endpoint` present → [`SeatClaim::UnmanagedEndpoint`]. Nothing local
    ///   to load; the endpoint's own `limits.concurrent_calls` bounds it.
    /// - the collection is EMPTY → [`SeatClaim::NoModel`]. The
    ///   short-circuit: a guaranteed no-op needs no model (ported
    ///   generically from the review verify seat, #1442), and this claim is
    ///   the mechanism by which an empty map performs ZERO model loads.
    ///   Being NoModel rather than "remote" also means it no longer
    ///   occupies a hosted-endpoint slot to do nothing.
    /// - a malformed collection, or absent residency hints (`model`,
    ///   `n_ctx`) → [`SeatClaim::LocalModelUnresolved`]. `run` owns
    ///   surfacing the real failure; the seat must never mask it, and now
    ///   says so loudly instead of failing open in silence.
    /// - otherwise → [`SeatClaim::LocalModel`].
    fn seat(
        &self,
        step: &Step,
        task: &Task,
        input: &BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> SeatClaim {
        let cfg: MapConfig = match load(step, ConfigKind::DispatchMap) {
            Ok(cfg) => cfg,
            Err(e) => return Self::seat_without_config(step, task, input, &e),
        };
        let call = &cfg.call;
        // (#2902 step 3) Same rule as `dispatch.single_shot`'s seat.
        match step_endpoint(call) {
            Ok(None) => {}
            Ok(Some(ep)) => return SeatClaim::UnmanagedEndpoint(crate::step_kinds::EndpointSlot::of(&ep)),
            Err(_) => return SeatClaim::UnmanagedEndpoint(crate::step_kinds::EndpointSlot::unresolved()),
        }
        match resolve_map_collection(step, task, input) {
            Ok(items) if items.is_empty() => return SeatClaim::NoModel,
            Ok(_) => {}
            Err(e) => {
                return SeatClaim::LocalModelUnresolved { reason: format!("collection: {e:#}") }
            }
        }
        let Some(min_ctx) = call.n_ctx.and_then(|n| n.as_u32()) else {
            return SeatClaim::LocalModelUnresolved { reason: "no usable config.n_ctx".to_string() };
        };
        // (#1442 ship-2b) `model_key` — the LOADABLE model key when it
        // differs from the wire `model` id. A local seat dispatches against
        // its darkmux-NAMESPACED identifier (`darkmux:<id>` as the wire
        // `model`), but the wave loader's `lms load` needs the bare model
        // key; without this override the loader would try to load the
        // namespaced string as if it were a model key.
        SeatClaim::LocalModel(darkmux_gestalt::Placement {
            model_key: call.model_key.clone().unwrap_or_else(|| call.model.clone()),
            identifier: local_dispatch_wire_model_id(call),
            min_ctx,
            // (#1442 gate C7) "step:<id>", consistent with the placement
            // provenance `dispatch.internal`'s seat claim uses.
            seat: format!("step:{}", step.id),
        })
    }

}

/// Runs a shell command from `Step.config`. Required: `command`
/// (string, passed to `sh -c`). Optional: `cwd` (string) — see
/// [`resolve_shell_cwd`] for the full resolution order, which also reads
/// the shared `workdir` vocabulary (`Task.workdir`, then a step config
/// `workdir` — the same tier order `dispatch_opts_for` uses for that key)
/// and refuses the step rather than silently inheriting a process working
/// directory that has been removed (#2532). Every dependency's output is
/// exposed as an env var
/// `DARKMUX_STEP_INPUT_<SANITIZED-DEP-ID>` (non-alphanumeric bytes in
/// the dependency id become `_`) so a shell step can consume prior
/// output without darkmux having to parse the command's own
/// substitution syntax. `DARKMUX_BIN` is exported too — the absolute path
/// of the darkmux running the mission, for a command that has to call
/// darkmux back. A non-zero exit is a loud `Err` carrying stdout
/// + stderr, never a silently-`Ok` failed command.
pub struct ProceduralShellStepKind;

/// (#2532) Resolve the directory a `procedural.shell` command runs in.
///
/// Priority, most specific first:
/// 1. `Step.config.cwd` — this kind's OWN key, and the only one of the
///    three that has no counterpart anywhere else: there is no
///    `Task.cwd`, so nothing can outrank it and its pre-#2532 behavior
///    ("an explicit `cwd` on the step is where the command runs") is
///    preserved exactly. `src/acp_panel.rs`'s `apply_default_cwd` writes
///    this key, so the panel's session directory lands here.
/// 2. `Task.workdir` — the mission's own tree root (e.g. a coder-phase
///    worktree).
/// 3. `Step.config.workdir` — the shared `workdir` vocabulary's
///    step-level tier.
/// 4. The process's own ambient working directory, IF it still exists.
///    Unset `cwd` on a `Command` inherits this already, so returning
///    `None` here is a no-op change for every step that has one of
///    these — which is most of them: `curl`/`sysctl`/`vm_stat`/`git
///    --version`-shaped commands never needed a real cwd, and refusing
///    them here would be a regression with no bug behind it.
///
/// **Tiers 2 and 3 are in THAT order deliberately, and it is pinned**
/// (`procedural_shell_task_workdir_outranks_a_step_config_workdir`).
/// `workdir` is shared vocabulary, so it has to mean one thing across
/// kinds: `dispatch_opts_for` resolves it `task.workdir.clone()
/// .or_else(|| cfg.workdir)` — Task first — and
/// `step_kinds::types`'s `StepKind` doc states the same contract ("a
/// dispatch-shaped step kind sources its assignment from THESE fields
/// first, falling back to `Step.config` only when the Task leaves a field
/// unset"). A step-config-first order here would have made one key mean
/// two different things depending on which kind read it. (That doc's next
/// sentence — "purely-procedural step kinds ignore `task` entirely" — is
/// what #2532 changes, and it is updated in place; this kind now reads
/// exactly one Task field, `workdir`, and nothing else.)
///
/// Nothing shipped changes tier today: the only production `workdir` on a
/// `procedural.shell` step is the one `review.json`'s `create-mod` task
/// GROWS into every step's config (`grow.config`'s keys are merged into
/// each step's config — see `mission_config::grow`), and no launcher sets
/// a `Task.workdir` on that task (`build_launch_params` only overrides the
/// tasks declaring `mission.coder`/`mission.verify`), so tier 3 is what
/// that step resolves through either way.
///
/// **Validation is the shared one, not a local `is_dir()`.**
/// `darkmux_types::workdir::validate_workdir` is where every other
/// operator-supplied workdir in darkmux is checked (#2302 — its own doc
/// names "the step's `config.workdir`" as one of its inputs). It refuses a
/// path traversing an operator-named symlink, distinguishes "does not
/// exist" from "cannot be resolved" (a bare `is_dir()` reports a
/// permissions error as absence), and returns the CANONICAL path, which is
/// what is handed to `Command::current_dir` so the directory the command
/// runs in is the explicit one that was validated.
///
/// The one case that changes for a step with no config at all: when NONE
/// of the above names a directory AND the ambient directory has been
/// removed (routine here, since the whole workflow is worktrees that get
/// deleted — #2532), this refuses the step with a clear reason instead of
/// handing `sh` a working directory that no longer exists. Measured before
/// this fix: a plain command still exits 0 but writes `shell-init: error
/// retrieving current directory` to stderr (which some panels in this
/// project render as failure), while `git status` exits 128 and a relative
/// `ls` exits 1 — three different silent-ish failure shapes for what is
/// really one config problem.
///
/// **A missing workdir is an ERROR here, not a skip — deliberately, and
/// the sibling kind in the same task disagrees on purpose.** `mods.gate`
/// treats a missing/unresolvable `config.workdir` as a named
/// `gate_skipped_reason` (its module doc, MUST FIX B: an operator reading
/// a mod's gate must be able to tell "the change is bad" from "the gate
/// itself couldn't run" at a glance). That distinction exists because
/// `mods.gate` writes a per-mod VERDICT that a reader would otherwise
/// misread as a judgment about the change. `procedural.shell` writes no
/// verdict — it has one outcome, its command's, and a command that never
/// ran because its directory was gone has no honest success to report. So
/// the two kinds legitimately differ: gate skips are DATA about a mod;
/// a shell step's missing directory is a config error about the step.
/// (In `review.json`'s `create-mod` task both kinds read the same grown
/// `workdir` and the shell step runs FIRST, so this error means the gate
/// never runs — the loud step error, naming the directory, is the signal;
/// a silent skip there would be the worse outcome.)
fn resolve_shell_cwd(step: &Step, task: &Task, cfg: &ShellConfig) -> Result<Option<std::path::PathBuf>> {
    if let Some(explicit) = cfg.cwd.as_deref() {
        return Ok(Some(validated_shell_cwd(step, "step config `cwd`", std::path::Path::new(explicit))?));
    }
    if let Some(path) = task.workdir.as_deref() {
        return Ok(Some(validated_shell_cwd(step, "the owning task's `workdir`", path)?));
    }
    if let Some(explicit) = cfg.workdir.as_deref() {
        return Ok(Some(validated_shell_cwd(step, "step config `workdir`", std::path::Path::new(explicit))?));
    }
    match std::env::current_dir() {
        // Already valid — `Command::current_dir` untouched inherits the
        // exact same directory, so this is behavior-identical to before
        // #2532 for every step that reaches here.
        //
        // Every TEST that reaches this branch reads a process-global, so
        // every one of them must be `#[serial_test::serial]` — see
        // `CwdGuard`'s doc in this file's test module for why reading is as
        // much a participation as writing.
        Ok(_) => Ok(None),
        Err(e) => bail!(
            "step `{}`: no `cwd`/`workdir` configured on the step or its task, and the \
             process's own working directory no longer exists ({e}) — routine when darkmux \
             was started from a worktree a later step in this workflow deleted. Set `cwd` \
             (or `workdir`) on the step, or a `workdir` on the owning task.",
            step.id
        ),
    }
}

/// Validate one resolved [`resolve_shell_cwd`] candidate through the SHARED
/// workdir validator, naming which tier it came from.
///
/// `source` is the operator-facing name of the key ("step config `cwd`"),
/// so a refusal says which of the three places to go fix — the validator's
/// own message names the workdir but cannot know which tier supplied it.
fn validated_shell_cwd(step: &Step, source: &str, raw: &std::path::Path) -> Result<std::path::PathBuf> {
    // An empty string is a config defect with its own message: routed
    // through the validator it renders as ``workdir path does not exist:
    // `` — empty backticks, which read as a darkmux bug rather than the
    // unfilled placeholder (a `grow.config` value that resolved to nothing)
    // it almost always is.
    if raw.as_os_str().is_empty() {
        bail!(
            "step `{}`: {source} is set but empty — name a directory, or remove the key \
             to run from the process's own working directory",
            step.id
        );
    }
    darkmux_types::workdir::validate_workdir(raw)
        .with_context(|| format!("step `{}`: resolving {source} ({})", step.id, raw.display()))
}

impl StepKind for ProceduralShellStepKind {
    /// (#2312) An untyped producer: its output is text, not a wrapped body,
    /// so it satisfies only a consumer that asks for [`labels::TEXT`].
    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::data(labels::TEXT)];
        &PORTS
    }

    /// (#2394) [`SeatClaim::NoModel`] — this kind runs an operator-supplied shell command and
    /// speaks to no model at all. Before this hook it said nothing, and
    /// silence classified it as a hosted-endpoint dispatch: a wave of these
    /// queued one at a time behind a serial endpoint, which a mission
    /// launch sets to 1. They now run on the dispatch-free track under
    /// `runtime.dispatch_free_concurrency`.
    fn seat(
        &self,
        _step: &Step,
        _task: &Task,
        _input: &BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> SeatClaim {
        SeatClaim::NoModel
    }

    fn id(&self) -> &'static str {
        ConfigKind::ProceduralShell.id()
    }

    fn display_name(&self) -> &'static str {
        "Shell"
    }

    /// (#1979) `None` — the documented no-dispatch opt-out. This kind
    /// performs no model work, so it owns no dispatch session; its only
    /// records are the scheduler's own task-scoped step-lifecycle bookends,
    /// which every kind gets and which a consumer adds once. Named on the
    /// no-dispatch list in `step_kinds::registry`'s conformance test, so
    /// this stays a stated choice rather than an unimplemented default.
    fn session_scope(&self) -> SessionScope {
        SessionScope::None
    }

    /// (#2577) The one kind that legitimately does — see
    /// [`CwdPolicy::AmbientWithRefusal`]'s own doc, and `resolve_shell_cwd`
    /// below for the resolution chain and the refusal it produces when the
    /// ambient directory has vanished.
    fn cwd_policy(&self) -> CwdPolicy {
        CwdPolicy::AmbientWithRefusal
    }

    fn run(&self, step: &Step, task: &Task, input: &BTreeMap<String, String>, _ctx: &StepRunCtx) -> Result<StepOutcome> {
        let cfg: ShellConfig = load(step, ConfigKind::ProceduralShell)?;
        let cwd = resolve_shell_cwd(step, task, &cfg)?;

        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(&cfg.command);
        if let Some(cwd) = &cwd {
            cmd.current_dir(cwd);
        }
        // (#3074) Declared values first, so the darkmux-set variables below
        // (`DARKMUX_STEP_INPUT_*`, `DARKMUX_BIN`) win over a clashing name.
        for (name, value) in cfg.env.iter().flatten() {
            match value {
                serde_json::Value::String(text) => cmd.env(name, text),
                other => cmd.env(name, other.to_string()),
            };
        }
        for (dep_id, output) in input {
            let env_key = sanitize_env_key(dep_id);
            cmd.env(format!("DARKMUX_STEP_INPUT_{env_key}"), output);
        }
        // (#2310 P4e) `DARKMUX_BIN` — THIS darkmux, by absolute path, so a
        // step command that has to ask darkmux something (`review`'s
        // wait step polls `mod list --for`) calls the binary running the
        // mission rather than whatever `darkmux` on `PATH` resolves to. A
        // step spawned from `cargo run`, from a worktree's `target/`, or
        // from a launcher whose `PATH` carries no darkmux at all would
        // otherwise fail on a name lookup that has nothing to do with the
        // step's own work. Set only when `current_exe` resolves; a command
        // written as `"${DARKMUX_BIN:-darkmux}"` still works when it does
        // not.
        //
        // (#2310 P4e review, item 8) Two cases where this is set but not
        // the darkmux an operator means, both of which the fallback CANNOT
        // rescue because the variable is present, just wrong:
        //
        //  - **Under `cargo test`** it is the TEST HARNESS binary, not
        //    darkmux. A step command that shells out to it gets a test
        //    runner. Harmless for the in-process kind tests (which only
        //    read the variable), and the integration tests that exercise a
        //    real command set `DARKMUX_BIN` themselves to
        //    `assert_cmd::cargo::cargo_bin("darkmux")` — but a future test
        //    that lets this default through would be testing the harness.
        //  - **After an in-place reinstall** (`cargo install` renames, but
        //    a `cp` over a running binary does not) `current_exe` can name
        //    a path that no longer exists. The command then fails on a
        //    missing file rather than falling back to `PATH`.
        //
        // Both are narrower than the failure this closes (a `PATH` with no
        // darkmux on it at all, which is the ordinary case for a worktree
        // build), so it stays — named here rather than silently inherited.
        if let Ok(exe) = std::env::current_exe() {
            cmd.env("DARKMUX_BIN", exe);
        }

        // (#2361, swarm S4-4) BOUNDED, in its own process group, and
        // registered with the child registry — see
        // `crate::bounded_command`'s module doc. An unbounded `.output()`
        // here pinned the mission Active with no way for a caught
        // SIGTERM/SIGINT to reach the child.
        let timeout = crate::bounded_command::configured_timeout();
        match crate::bounded_command::run_bounded(cmd, timeout) {
            crate::bounded_command::Bounded::Finished { success, code, stdout, stderr } => {
                if !success {
                    anyhow::bail!(
                        "step `{}`: command exited with {:?}\nstdout: {}\nstderr: {}",
                        step.id,
                        code,
                        String::from_utf8_lossy(&stdout),
                        String::from_utf8_lossy(&stderr),
                    );
                }
                Ok(StepOutcome {
                    output: String::from_utf8_lossy(&stdout).to_string(),
                    flow_records: Vec::new(),
                    degraded: None,
                })
            }
            // A killed command produced no output this step could hand on,
            // so — unlike `mods.gate`, where a hung `test_command` is
            // recorded as gate DATA — this is a step error naming the bound.
            crate::bounded_command::Bounded::TimedOut { seconds } => anyhow::bail!(
                "step `{}`: command exceeded {seconds}s and was killed \
                 (`runtime.step_command_timeout_seconds`)",
                step.id
            ),
            crate::bounded_command::Bounded::Interrupted => anyhow::bail!(
                "step `{}`: interrupted before the command finished; the child was killed",
                step.id
            ),
            // (#2532) The resolved working directory is NAMED here. A spawn
            // that fails because the directory vanished between resolution
            // and `fork` renders `No such file or directory` — byte-identical
            // to `sh` itself being missing, which is exactly the confusion
            // this change exists to end. `<inherited>` when nothing was set
            // (the ambient tier), so the message never implies a `cwd` the
            // step did not have.
            crate::bounded_command::Bounded::SpawnFailed(e) => {
                let where_ = cwd
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<inherited from the process>".to_string());
                Err(anyhow::Error::new(e))
                    .with_context(|| format!("step `{}`: spawning shell command in {where_}", step.id))
            }
        }
    }
}

fn sanitize_env_key(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect()
}

/// A no-op step kind for graph-structure testing (topological ordering,
/// concurrency, cycle detection, reachability) without touching any real
/// model or process — the scheduler's own test suite's primary fixture
/// kind. Optional `Step.config.output` (string) overrides the returned
/// output text; defaults to the step's own id (a cheap, inspectable
/// per-step marker for assertions like "did B and C both run before D").
pub struct ProceduralNoopStepKind;

impl StepKind for ProceduralNoopStepKind {
    /// (#2394) [`SeatClaim::NoModel`] — this kind returns a fixed string and
    /// speaks to no model at all. Before this hook it said nothing, and
    /// silence classified it as a hosted-endpoint dispatch: a wave of these
    /// queued one at a time behind a serial endpoint, which a mission
    /// launch sets to 1. They now run on the dispatch-free track under
    /// `runtime.dispatch_free_concurrency`.
    fn seat(
        &self,
        _step: &Step,
        _task: &Task,
        _input: &BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> SeatClaim {
        SeatClaim::NoModel
    }

    fn id(&self) -> &'static str {
        ConfigKind::ProceduralNoop.id()
    }

    fn display_name(&self) -> &'static str {
        "No-op"
    }

    /// (#1979) `None` — the documented no-dispatch opt-out. This kind
    /// performs no model work, so it owns no dispatch session; its only
    /// records are the scheduler's own task-scoped step-lifecycle bookends,
    /// which every kind gets and which a consumer adds once. Named on the
    /// no-dispatch list in `step_kinds::registry`'s conformance test, so
    /// this stays a stated choice rather than an unimplemented default.
    fn session_scope(&self) -> SessionScope {
        SessionScope::None
    }


    fn run(&self, step: &Step, _task: &Task, _input: &BTreeMap<String, String>, _ctx: &StepRunCtx) -> Result<StepOutcome> {
        let cfg: NoopConfig = load(step, ConfigKind::ProceduralNoop)?;
        let output = cfg.output.unwrap_or_else(|| step.id.clone());
        Ok(StepOutcome {
            output,
            flow_records: Vec::new(),
            degraded: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A step execution's bookend arms its abort terminal when the execution
    /// starts, before any stop. An operator's signal that then ends the
    /// execution must still be named on that terminal when it is written, or
    /// the run reads "error" on every flow-derived view while the run's own
    /// record reads "aborted".
    #[test]
    #[serial_test::serial] // the interrupt flag is process-wide
    fn an_armed_abort_written_after_an_operator_stop_names_the_stop() {
        // (#3100) It raises the process-wide interrupt flag.
        darkmux_types::run_in_own_process!();
        darkmux_types::interrupt::reset_for_test();
        let session = darkmux_types::session_id::SessionId::task(crate::test_run(), "t1");
        let execution = darkmux_types::execution_id::ExecutionId::mint();
        let rec = |payload| {
            darkmux_flow::FlowRecord::for_execution_with(
                &session,
                &execution,
                darkmux_flow::Level::Info,
                darkmux_flow::Category::Work,
                darkmux_flow::Stage::Dispatch,
                payload,
                "s1",
            )
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = StepRunCtx::new(crate::test_run(), Some(tx), None, std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()));
        let bookend = StepBookend::new(
            Some(&ctx),
            rec(darkmux_flow::Payload::DispatchStart(DispatchStartPayload::default())),
            rec(darkmux_flow::Payload::DispatchError(DispatchEndPayload::aborted(None))),
        );
        darkmux_types::interrupt::simulate_sigterm_for_test();
        drop(bookend);
        darkmux_types::interrupt::reset_for_test();
        drop(ctx);
        let stops: Vec<Option<String>> = rx
            .into_iter()
            .filter_map(|sig| match sig {
                crate::step_kinds::WaveSignal::Record(darkmux_flow::FlowRecord {
                    payload: Some(darkmux_flow::Payload::DispatchError(p)), ..
                }) => Some(p.stop_reason),
                _ => None,
            })
            .collect();
        assert_eq!(stops, [Some("SIGTERM".to_string())]);
    }
    use std::sync::Arc;
    use serde_json::json;
    use tempfile::TempDir;

    fn step(id: &str, kind: &str, config: serde_json::Value) -> Step {
        Step {
            id: id.to_string(),
            task_id: "t1".to_string(),
            gate: None,
            kind: kind.to_string(),
            status: crate::types::NodeStatus::Planned,
            config,
            started_ts: None,
            completed_ts: None,
            output: None,
            stop_reason: None,
        }
    }

    /// A Task with no resource assignment (#1230/#1341) — the default
    /// fixture for tests that don't exercise Task-sourced
    /// `role_id`/`profile_name`/`workdir`/`image` (see
    /// `dispatch_internal_sources_role_id_from_task` for a test that
    /// does).
    fn empty_task() -> Task {
        Task {
            run_on: crate::types::default_run_on(),
            id: "t1".to_string(),
            phase_id: "p1".to_string(),
            description: "test task".to_string(),
            display_name: None,
            step_ids: vec!["s1".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        }
    }

    /// Task `t1`'s session in the test run: what a task-scoped seat's
    /// records land under.
    fn task_session() -> darkmux_types::session_id::SessionId {
        darkmux_types::session_id::SessionId::task(crate::test_run(), "t1")
    }

    /// (#1530 Packet 3a) A bare `StepRunCtx` — no emitter/bucket/override,
    /// an empty `ArtifactBus` — for tests that call `seat()` directly
    /// (bypassing the scheduler, which is the only production caller that
    /// materializes a real bus). None of `seat()`'s Tier 1 builtin
    /// implementations read the bus, so an empty one is sufficient here.
    fn bare_ctx() -> StepRunCtx {
        StepRunCtx::new(crate::test_run(), None, None, std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()))
    }

    // ── #2614 review: resume_precheck / message ordering ────────────────

    /// (#2614 review, MUST FIX) `StepKind::resume_precheck` is what closes
    /// the checkpoint gate on the mission-launcher / panel entry points
    /// #2585 never reached — proved here by driving the SCHEDULER-FACING
    /// trait method directly, on the real production kind, with NO
    /// `run_step_graph`/Docker/model involved.
    #[serial_test::serial]
    #[test]
    fn dispatch_internal_resume_precheck_refuses_a_missing_checkpoint() {
        // (#2718) Every darkmux write destination, pinned for this test.
        // Measured before this line existed: a full `-p darkmux-crew --lib`
        // run with all twelve state variables exported to a fresh root still
        // wrote a findings file, 29 flow records across 12 (action, source,
        // session) groups, 214 hash-chained audit records and a liveness log
        // into that root — the operator's real tree in an ordinary shell.
        let _isolated = darkmux_types::test_isolation::IsolatedState::new();
        let resume_from = TempDir::new().unwrap(); // no checkpoint.json written
        let s = step(
            "s1",
            "dispatch.internal",
            json!({
                "role_id": "coder",
                "message": "resume please",
                "resume_from": resume_from.path().to_str().unwrap(),
            }),
        );
        let err = DispatchInternalStepKind
            .resume_precheck(&s, &empty_task(), &BTreeMap::new(), &bare_ctx())
            .expect_err("a --resume-from with no checkpoint must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("RESUME CHECKPOINT NOT FOUND"), "{msg}");
        assert!(msg.starts_with("darkmux dispatch: RESUME CHECKPOINT NOT FOUND"), "one clear prefix: {msg}");
    }

    #[test]
    fn dispatch_internal_resume_precheck_is_a_noop_with_no_resume_from_configured() {
        let s = step("s1", "dispatch.internal", json!({"role_id": "coder", "message": "hi"}));
        DispatchInternalStepKind
            .resume_precheck(&s, &empty_task(), &BTreeMap::new(), &bare_ctx())
            .expect("no `resume_from` key at all must never refuse");
    }

    /// (#2614 review, "Also fix" — wrong problem surfaced) A role that
    /// resolves to the bare hosted single-shot path (a remote profile, no
    /// tools) can NEVER honor `--resume-from` — regardless of whether the
    /// checkpoint at `resume_from` is even valid. `resume_from` here has
    /// NO `checkpoint.json` (deliberately invalid), the same repro shape
    /// as the sibling test above — so if `validate_resume_checkpoint_
    /// content` ran BEFORE `refuse_resume_on_bare_hosted_path` inside
    /// `resume_precheck`, this would surface "RESUME CHECKPOINT NOT
    /// FOUND" instead: the operator fixes a checkpoint that could never
    /// have helped, redispatches, and only then discovers resume was
    /// never possible on this role shape at all. `pr-reviewer` is a real
    /// built-in, tool-less role (same fixture
    /// `dispatch_unmanaged_refuses_resume_from_before_the_http_call` in
    /// `dispatch_internal_tests.rs` uses) pinned at a `config_path`
    /// profiles registry whose only profile targets a REMOTE endpoint —
    /// no HTTP mock needed, since a correct refusal never dials it.
    #[test]
    #[serial_test::serial]
    fn dispatch_internal_resume_precheck_names_the_unmanaged_single_shot_path_not_the_checkpoint() {
        // (#2718) Every darkmux write destination, pinned for this test.
        // Measured before this line existed: a full `-p darkmux-crew --lib`
        // run with all twelve state variables exported to a fresh root still
        // wrote a findings file, 29 flow records across 12 (action, source,
        // session) groups, 214 hash-chained audit records and a liveness log
        // into that root — the operator's real tree in an ordinary shell.
        let _isolated = darkmux_types::test_isolation::IsolatedState::new();
        let home = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", home.path()) };

        let registry_dir = TempDir::new().unwrap();
        let pf = registry_dir.path().join("profiles.json");
        std::fs::write(
            &pf,
            r#"{"profiles":{"cloud":{"models":[
                    {"id":"gpt-remote","n_ctx":100000,"endpoint":"mock"}
                ]}},
                "endpoints":{"mock":{"url":"http://127.0.0.1:1"}},
                "default_profile":"cloud"}"#,
        )
        .unwrap();

        let resume_from = TempDir::new().unwrap(); // deliberately no checkpoint.json

        let s = step(
            "s1",
            "dispatch.internal",
            json!({
                "role_id": "pr-reviewer",
                "message": "resume please",
                "resume_from": resume_from.path().to_str().unwrap(),
                "config_path": pf.to_str().unwrap(),
            }),
        );

        let result = DispatchInternalStepKind.resume_precheck(&s, &empty_task(), &BTreeMap::new(), &bare_ctx());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }

        let err = result.expect_err("a bare-hosted role with --resume-from must refuse");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not supported on the unmanaged-endpoint single-shot dispatch path"),
            "must name the unmanaged-endpoint single-shot path as the reason: {msg}"
        );
        assert!(
            !msg.contains("RESUME CHECKPOINT NOT FOUND"),
            "must not surface a checkpoint-content error first for a role this can never \
             resume regardless of checkpoint validity: {msg}"
        );
    }

    // ── (#2570) load/wire agreement for dispatch.single_shot / dispatch.map ──
    //
    // Before this fix, `seat()` derived `darkmux:<key>` (or an explicit
    // `identifier` override) to claim residency, and `run`/`run_map` then put
    // the BARE `config.model` string on the wire, unconditionally — a local
    // step naming a bare model with no override loaded one identifier and
    // addressed another. The pure tests below pin `local_dispatch_wire_model_id`
    // (the one function both `seat()`s and both `run`/`run_map` now share)
    // against `seat()`'s own claimed `Placement.identifier`; the httpmock
    // tests red-prove the ACTUAL dispatch — the thing #2570 is about — by
    // pointing `DARKMUX_LMSTUDIO_URL` at a mock that only answers a request
    // whose JSON body names the namespaced identifier.

    /// The model-call keys of a single-shot or map test step.
    fn call_of(step: &Step) -> ModelCallConfig {
        crate::step_config::model_call(&step.kind, &step.config).expect("a model-call step with a model")
    }

    #[test]
    fn local_dispatch_wire_model_id_matches_what_seat_claims_without_an_override() {
        let single = step(
            "s1",
            "dispatch.single_shot",
            json!({ "model": "qwen3-4b", "user": "hi", "n_ctx": 8192 }),
        );
        let SeatClaim::LocalModel(placement) =
            DispatchSingleShotStepKind.seat(&single, &empty_task(), &BTreeMap::new(), &bare_ctx())
        else {
            panic!("expected LocalModel");
        };
        assert_eq!(
            local_dispatch_wire_model_id(&call_of(&single)),
            placement.identifier,
            "the wire helper must derive the identical identifier seat() claimed residency \
             under, or load and wire disagree"
        );
        assert_eq!(placement.identifier, "darkmux:qwen3-4b");

        let map = map_step(json!({
            "model": "qwen3-4b",
            "user_template": "check {item}",
            "n_ctx": 8192,
            "collection": ["a"],
        }));
        let SeatClaim::LocalModel(placement) =
            DispatchMapStepKind.seat(&map, &empty_task(), &BTreeMap::new(), &bare_ctx())
        else {
            panic!("expected LocalModel");
        };
        assert_eq!(local_dispatch_wire_model_id(&call_of(&map)), placement.identifier);
        assert_eq!(placement.identifier, "darkmux:qwen3-4b");
    }

    #[test]
    fn local_dispatch_wire_model_id_honors_an_explicit_identifier_override() {
        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({ "model": "qwen3-4b", "identifier": "my-own-alias", "user": "hi", "n_ctx": 8192 }),
        );
        assert_eq!(local_dispatch_wire_model_id(&call_of(&s)), "my-own-alias");
        let SeatClaim::LocalModel(placement) =
            DispatchSingleShotStepKind.seat(&s, &empty_task(), &BTreeMap::new(), &bare_ctx())
        else {
            panic!("expected LocalModel");
        };
        assert_eq!(
            placement.identifier, "my-own-alias",
            "the wire helper and seat()'s own claim must still agree with an override present"
        );
    }

    /// The sibling of the test above for `dispatch.map` — the no-override
    /// case above already covers both kinds together, but the OVERRIDE path
    /// was only pinned for `dispatch.single_shot`, leaving `dispatch.map`'s
    /// own `identifier` read in `local_dispatch_wire_
    /// model_id` unpinned for the override branch specifically.
    #[test]
    fn map_local_dispatch_wire_model_id_honors_an_explicit_identifier_override() {
        let m = map_step(json!({
            "model": "qwen3-4b",
            "identifier": "my-own-alias",
            "user_template": "check {item}",
            "n_ctx": 8192,
            "collection": ["a"],
        }));
        assert_eq!(local_dispatch_wire_model_id(&call_of(&m)), "my-own-alias");
        let SeatClaim::LocalModel(placement) =
            DispatchMapStepKind.seat(&m, &empty_task(), &BTreeMap::new(), &bare_ctx())
        else {
            panic!("expected LocalModel");
        };
        assert_eq!(
            placement.identifier, "my-own-alias",
            "the wire helper and seat()'s own claim must still agree with an override present"
        );
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL
    fn dispatch_single_shot_local_addresses_the_namespaced_identifier_it_would_load() {
        // (#2570) Red-proves the actual bug: before the fix, this step
        // dispatched with `"model": "qwen3-4b"` on the wire (the bare
        // `config.model`, verbatim) while `seat()` claimed residency for
        // `darkmux:qwen3-4b` — so a real LMStudio, resolving the bare key,
        // could answer with a co-resident user-loaded copy at an unknown
        // context (the #1135 shape) instead of the instance darkmux just
        // loaded. The mock below only answers a request whose body names
        // the NAMESPACED identifier; a bare-key request gets no matching
        // mock and the dispatch fails.
        use httpmock::prelude::*;
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .json_body_partial(r#"{"model": "darkmux:qwen3-4b"}"#);
            then.status(200).header("content-type", "application/json").json_body(json!({
                "id": "mock-1",
                "object": "chat.completion",
                "created": 0,
                "model": "qwen3-4b",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
            }));
        });

        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev = std::env::var(url_key).ok();
        unsafe {
            std::env::set_var(url_key, server.base_url());
        }

        let s = step("s1", "dispatch.single_shot", json!({ "model": "qwen3-4b", "user": "hi" }));
        let out = DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test());

        unsafe {
            match prev {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }

        let out = out.expect(
            "a local dispatch.single_shot with no identifier override must address \
             `darkmux:qwen3-4b` — the mock only answers that body, so a failure here means the \
             bare key went out instead",
        );
        assert_eq!(out.output, "ok");
        mock.assert_hits(1);
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL
    fn dispatch_map_local_addresses_the_namespaced_identifier_it_would_load() {
        // (#2570) Same red-prove as the single_shot test above, for the
        // per-item local dispatch loop `dispatch.map` runs.
        use httpmock::prelude::*;
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .json_body_partial(r#"{"model": "darkmux:qwen3-4b"}"#);
            then.status(200).header("content-type", "application/json").json_body(json!({
                "id": "mock-1",
                "object": "chat.completion",
                "created": 0,
                "model": "qwen3-4b",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
            }));
        });

        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev = std::env::var(url_key).ok();
        unsafe {
            std::env::set_var(url_key, server.base_url());
        }

        let s = map_step(json!({
            "model": "qwen3-4b",
            "user_template": "check {item}",
            "collection": ["a", "b"],
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test());

        unsafe {
            match prev {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }

        let out = out.expect("dispatch.map's local per-item dispatch must not fail outright");
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 2);
        assert!(
            results.iter().all(|r| r.ok && r.content == "ok"),
            "every item must have addressed `darkmux:qwen3-4b` — the mock only answers that \
             body, so a bare-key item shows up here as a failure: {results:?}"
        );
        mock.assert_hits(2);

        // (MUST FIX 5) The per-item, aggregate, and token-telemetry RECORDS
        // this run also produced (returned batched here since `ctx` is
        // `None`) must themselves carry the namespaced identifier, not just
        // the wire body the mock above already proves. Reverting any of
        // `item_record`'s / `aggregate_record`'s / the `telemetry.tokens`
        // push's `wire_model.as_ref()` argument back to the bare `model`
        // leaves the dispatch itself succeeding (the mock only inspects the
        // wire body) while these records silently go back to lying about
        // what actually ran.
        let item_records: Vec<&darkmux_flow::FlowRecord> = out
            .flow_records
            .iter()
            .filter(|r| r.action == darkmux_flow::FlowAction::StepResult && r.payload_json().get("index").is_some())
            .collect();
        assert_eq!(item_records.len(), 2, "one `step result` record per item: {:?}", out.flow_records);
        for rec in &item_records {
            assert_eq!(
                rec.model.as_deref(),
                Some("darkmux:qwen3-4b"),
                "item_record must carry the namespaced identifier the item actually \
                 addressed: {rec:?}"
            );
        }

        let aggregate = out
            .flow_records
            .iter()
            .find(|r| r.action == darkmux_flow::FlowAction::StepResult && r.payload_json().get("items_in").is_some())
            .expect("aggregate_record must be present");
        assert_eq!(
            aggregate.model.as_deref(),
            Some("darkmux:qwen3-4b"),
            "aggregate_record must carry the namespaced identifier: {aggregate:?}"
        );

        let telemetry_records: Vec<&darkmux_flow::FlowRecord> =
            out.flow_records.iter().filter(|r| r.action == darkmux_flow::FlowAction::TelemetryTokens).collect();
        assert_eq!(
            telemetry_records.len(),
            2,
            "one telemetry.tokens record per item that reported usage (the mock reports usage \
             for both): {:?}",
            out.flow_records
        );
        for rec in &telemetry_records {
            assert_eq!(
                rec.model.as_deref(),
                Some("darkmux:qwen3-4b"),
                "telemetry.tokens record must carry the namespaced identifier: {rec:?}"
            );
        }
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL
    fn dispatch_single_shot_local_streaming_bookends_carry_the_namespaced_identifier() {
        // (MUST FIX 5) `dispatch.single_shot`'s local arm produces NO
        // `flow_records` via the batched `run()` path (its "step result"
        // record only exists on the HOSTED branch) — the only place a local
        // dispatch's bookends are observable at all is the STREAMING path,
        // through the scheduler's live-emission channel. This is the same
        // streaming-plus-channel shape
        // `dispatch_map_hosted_emits_liveness_bookends_carrying_the_endpoint`
        // already uses. Reverting the `ExecutionBookends`'
        // `model: wire_model.as_ref()` back to the bare `model` leaves
        // the dispatch itself succeeding (the mock only inspects the wire
        // body) while the liveness bookends silently go back to naming the
        // wrong model.
        use httpmock::prelude::*;
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .json_body_partial(r#"{"model": "darkmux:qwen3-4b"}"#);
            then.status(200).header("content-type", "application/json").json_body(json!({
                "id": "mock-1",
                "object": "chat.completion",
                "created": 0,
                "model": "qwen3-4b",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
            }));
        });

        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev = std::env::var(url_key).ok();
        unsafe {
            std::env::set_var(url_key, server.base_url());
        }

        let s = step("s1", "dispatch.single_shot", json!({ "model": "qwen3-4b", "user": "hi" }));
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = StepRunCtx::new(crate::test_run(), 
            Some(tx),
            None,
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );
        let result = DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &ctx);
        drop(ctx);

        unsafe {
            match prev {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }

        result.expect(
            "a local dispatch.single_shot with no identifier override must address \
             `darkmux:qwen3-4b` — the mock only answers that body",
        );
        mock.assert_hits(1);

        let emitted: Vec<darkmux_flow::FlowRecord> = rx
            .into_iter()
            .filter_map(|sig| match sig {
                crate::step_kinds::WaveSignal::Record(r) => Some(r),
                _ => None,
            })
            .collect();
        let bookends: Vec<&darkmux_flow::FlowRecord> =
            emitted.iter().filter(|r| matches!(r.action, darkmux_flow::FlowAction::DispatchStart | darkmux_flow::FlowAction::DispatchComplete | darkmux_flow::FlowAction::DispatchError)).collect();
        assert_eq!(
            bookends.len(),
            2,
            "expected exactly a start and a complete bookend: {emitted:?}"
        );
        for rec in &bookends {
            assert_eq!(
                rec.model.as_deref(),
                Some("darkmux:qwen3-4b"),
                "bookend record must carry the namespaced identifier the dispatch actually \
                 addressed, not the bare config.model: {rec:?}"
            );
        }
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL
    fn dispatch_single_shot_local_failure_names_the_lost_residency() {
        // (Also fix, #2570 class) When the resident darkmux-namespaced
        // instance is gone, LMStudio answers with a "not found"-shaped
        // error instead of silently JIT-loading a bare-key copy. Before
        // `with_residency_lost_hint` was wired into this local arm, that
        // surfaced as a bare, unexplained error; this pins that the hint
        // (`dispatch_internal::residency_lost_detail`) is actually attached
        // here, the same way it already is on the top-level local dispatch
        // path (#2240).
        use httpmock::prelude::*;
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(400).header("content-type", "application/json").json_body(json!({
                "error": { "message": "Model \"darkmux:qwen3-4b\" not found" },
            }));
        });

        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev = std::env::var(url_key).ok();
        unsafe {
            std::env::set_var(url_key, server.base_url());
        }

        let s = step("s1", "dispatch.single_shot", json!({ "model": "qwen3-4b", "user": "hi" }));
        let out = DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test());

        unsafe {
            match prev {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }

        let err = out.expect_err("a 400 'not found' response must surface as an Err");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("darkmux dispatches only to its OWN namespaced instance"),
            "the local arm must attach residency_lost_detail's hint, not just the bare LMStudio \
             error: {msg}"
        );
        mock.assert_hits(1);
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL
    fn dispatch_map_local_item_failure_names_the_lost_residency() {
        // (Also fix, #2570 class) Same as the single_shot test above, for
        // `dispatch.map`'s per-item local arm (`map_local_item`).
        use httpmock::prelude::*;
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(400).header("content-type", "application/json").json_body(json!({
                "error": { "message": "Model \"darkmux:qwen3-4b\" not found" },
            }));
        });

        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev = std::env::var(url_key).ok();
        unsafe {
            std::env::set_var(url_key, server.base_url());
        }

        let s = map_step(json!({
            "model": "qwen3-4b",
            "user_template": "check {item}",
            "collection": ["a"],
        }));
        let out = DispatchMapStepKind.run_map(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).map(|run| run.outcome);

        unsafe {
            match prev {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }

        let out = out.expect("the item pass completes even when an item fails");
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].ok, "the item must be marked failed: {results:?}");
        let err_text = results[0].error.as_deref().unwrap_or_default();
        assert!(
            err_text.contains("darkmux dispatches only to its OWN namespaced instance"),
            "map_local_item must attach residency_lost_detail's hint to the item error, not \
             just the bare LMStudio error: {err_text}"
        );
        mock.assert_hits(1);
    }

    // ─── Also fix (second review round): the DROP-PATH error bookend's
    //     model field, unpinned in both kinds ───────────────────────────

    #[test]
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL
    fn dispatch_single_shot_local_streaming_drop_path_error_bookend_carries_the_namespaced_identifier() {
        // (Also fix, second review round) The two streaming bookend tests
        // above (`..._local_streaming_bookends_carry_the_namespaced_
        // identifier`) only ever reach the CLEAN path: `bookend.close(...)`
        // fires with `wire_model.as_ref()`, so they pin the START and the
        // real COMPLETE record. `StepBookend`'s on_abort record — the
        // "dispatch error" the Drop impl emits when `run_single_shot`
        // returns early via `?` WITHOUT ever reaching `close` — is a
        // SEPARATE record (the abort half `ExecutionBookends::open` builds beside
        // the start, from the same `model`), and nothing
        // exercised it: mutating that call back to the bare `model` compiled
        // and left the whole suite green, because no test ever forced the
        // dispatch itself to fail on the STREAMING path.
        //
        // This does: same streaming+channel harness as the sibling test
        // above, but the mock answers 400 (matching `..._local_failure_
        // names_the_lost_residency`'s shape), so `single_shot_chat(&req)?`
        // at the end of `run_single_shot` returns early — `bookend.close`
        // is never called, and `StepBookend`'s Drop fires the on_abort
        // record into the channel instead.
        use httpmock::prelude::*;
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(400).header("content-type", "application/json").json_body(json!({
                "error": { "message": "Model \"darkmux:qwen3-4b\" not found" },
            }));
        });

        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev = std::env::var(url_key).ok();
        unsafe {
            std::env::set_var(url_key, server.base_url());
        }

        let s = step("s1", "dispatch.single_shot", json!({ "model": "qwen3-4b", "user": "hi" }));
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = StepRunCtx::new(crate::test_run(), 
            Some(tx),
            None,
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );
        let result = DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &ctx);
        drop(ctx);

        unsafe {
            match prev {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }

        result.expect_err("the mocked 400 must surface as an Err — this test needs the drop path, not the clean one");
        mock.assert_hits(1);

        let emitted: Vec<darkmux_flow::FlowRecord> = rx
            .into_iter()
            .filter_map(|sig| match sig {
                crate::step_kinds::WaveSignal::Record(r) => Some(r),
                _ => None,
            })
            .collect();
        let error_bookends: Vec<&darkmux_flow::FlowRecord> =
            emitted.iter().filter(|r| r.action == darkmux_flow::FlowAction::DispatchError).collect();
        assert_eq!(
            error_bookends.len(),
            1,
            "expected exactly the drop-emitted error bookend (no start-only or double-emit): \
             {emitted:?}"
        );
        assert_eq!(
            error_bookends[0].model.as_deref(),
            Some("darkmux:qwen3-4b"),
            "the DROP-PATH error bookend must carry the namespaced identifier the dispatch \
             actually addressed, not the bare config.model: {:?}",
            error_bookends[0]
        );
    }

    /// (#3121) A `dispatch.single_shot` whose call fails writes a
    /// `dispatch.error` naming that failure and how long the call ran, not the
    /// guard's "early return or panic". A signal stopping a mission landed
    /// here: `step.error` named the signal while the execution's own terminal
    /// called it a panic.
    #[test]
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL
    fn a_failed_single_shot_call_writes_its_own_error_into_the_execution_terminal() {
        use httpmock::prelude::*;
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(400).header("content-type", "application/json").json_body(json!({
                "error": { "message": "Model \"darkmux:qwen3-4b\" not found" },
            }));
        });
        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev = std::env::var(url_key).ok();
        unsafe {
            std::env::set_var(url_key, server.base_url());
        }
        let s = step("s1", "dispatch.single_shot", json!({ "model": "qwen3-4b", "user": "hi" }));
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = StepRunCtx::new(crate::test_run(),
            Some(tx),
            None,
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );
        let result = DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &ctx);
        drop(ctx);
        unsafe {
            match prev {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }
        let err = format!("{:#}", result.expect_err("the mocked 400 fails the step"));
        mock.assert_hits(1);
        let terminal = rx
            .into_iter()
            .filter_map(|sig| match sig {
                crate::step_kinds::WaveSignal::Record(r) => Some(r),
                _ => None,
            })
            .find(|r| r.action == darkmux_flow::FlowAction::DispatchError)
            .expect("the execution has a terminal");
        let payload = terminal.payload_json();
        let recorded = payload["error"].as_str().unwrap_or_default();
        assert!(!recorded.contains("early return or panic"), "the guard's fallback, not the cause: {payload}");
        assert!(recorded.contains("not found"), "names the failure the step returned ({err}): {payload}");
        assert!(payload["wall_ms"].is_u64(), "how long the call ran: {payload}");
    }

    /// (#3121) The guard's terminal is stamped when it is written, not when
    /// the guard opened: a terminal stamped with its start time reads as a
    /// zero-length execution.
    #[test]
    fn a_step_bookend_stamps_its_abort_terminal_when_it_writes_it() {
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = StepRunCtx::new(crate::test_run(),
            Some(tx),
            None,
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );
        let s = step("s1", "dispatch.single_shot", json!({ "model": "m", "user": "hi" }));
        let session = crate::test_session("sess-3121");
        let execution = ExecutionId::mint();
        let records = ExecutionBookends {
            kind: "dispatch.single_shot",
            session: &session,
            execution: &execution,
            step: &s,
            model: "m",
            endpoint_label: None,
        };
        let mut start = records.record(darkmux_flow::Level::Info, darkmux_flow::Payload::DispatchStart(records.start_payload(None)));
        start.ts = "2000-01-01T00:00:00Z".to_string();
        let mut abort = records.record(
            darkmux_flow::Level::Error,
            darkmux_flow::Payload::DispatchError(records.end_payload(0)),
        );
        abort.ts = "2000-01-01T00:00:00Z".to_string();
        drop(StepBookend::new(Some(&ctx), start, abort));
        drop(ctx);
        let terminal = rx
            .into_iter()
            .filter_map(|sig| match sig {
                crate::step_kinds::WaveSignal::Record(r) => Some(r),
                _ => None,
            })
            .find(|r| r.action == darkmux_flow::FlowAction::DispatchError)
            .expect("the dropped guard writes its terminal");
        let written = darkmux_flow::parse_ts_utc(&terminal.ts).expect("a record ts");
        let now = darkmux_flow::parse_ts_utc(&darkmux_flow::ts_utc_now()).unwrap();
        assert!(now - written <= 5, "stamped at write time, not at open: {}", terminal.ts);
    }

    #[serial_test::serial]
    #[test]
    fn dispatch_map_local_streaming_drop_path_error_bookend_carries_the_namespaced_identifier() {
        // (Also fix, second review round) `dispatch.map`'s twin of the test
        // above. Unlike `dispatch.single_shot`, `dispatch.map`'s per-item
        // failures are ALL isolated into `MapItemResult` by design (the
        // "per-item error isolation" policy this kind documents) — neither
        // `map_local_item` nor `map_hosted_item` ever propagates a `?` back
        // through `run_map`. So there is no ordinary dispatch failure that
        // reaches `StepBookend`'s Drop for this kind; the only route is a
        // genuine panic between `StepBookend::new` (bookend creation) and
        // `bookend.close` (after the per-item loop). This test drives that
        // panic through the SAME seam a real sibling failure
        // would use in production: the `MapDispatchOverride` test seam
        // (`StepRunCtx::dispatch_override`, already exercised in
        // `scheduler.rs`'s `dispatch_override_intercepts_dispatch_map_items_
        // on_the_worker_thread`) intercepts the per-item transport call —
        // panicking there is a stand-in for a transport-layer bug the
        // override seam exists to let tests reach without a live server.
        //
        // LOCAL (no `config.endpoint`) is required, not incidental: a
        // HOSTED item's `wire_model` and its bare `config.model` are the
        // SAME string (#2570 — hosted steps keep the bare model verbatim,
        // since an endpoint deployment name is never loaded into local
        // residency), so a hosted-only version of this test could not
        // distinguish "used wire_model" from "used the bare value" — the
        // exact mutation this test exists to catch is only observable on
        // the LOCAL arm, where `wire_model` is the namespaced
        // `darkmux:qwen3-4b` and the bare `config.model` is plain
        // `qwen3-4b`.
        let ovr: MapDispatchOverride =
            Arc::new(|_call: &OverrideDispatchCall<'_>| -> Result<crate::single_shot::SingleShotReply> {
                panic!("deliberate panic — this test needs run_map to exit via unwind, not `?`, \
                        to exercise StepBookend's Drop-emitted record");
            });

        let s = map_step(json!({
            "model": "qwen3-4b",
            "user_template": "check {item}",
            "collection": ["a"],
        }));
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = StepRunCtx::new(crate::test_run(), 
            Some(tx),
            Some(ovr),
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &ctx)
        }));
        drop(ctx);

        assert!(
            result.is_err(),
            "the override's panic must propagate out of run_streaming — this test needs the \
             UNWIND path, not a caught error, to reach StepBookend's Drop"
        );

        let emitted: Vec<darkmux_flow::FlowRecord> = rx
            .into_iter()
            .filter_map(|sig| match sig {
                crate::step_kinds::WaveSignal::Record(r) => Some(r),
                _ => None,
            })
            .collect();
        let error_bookends: Vec<&darkmux_flow::FlowRecord> =
            emitted.iter().filter(|r| r.action == darkmux_flow::FlowAction::DispatchError).collect();
        assert_eq!(
            error_bookends.len(),
            1,
            "expected exactly the drop-emitted error bookend (no clean complete — run_map \
             panicked before reaching bookend.close): {emitted:?}"
        );
        assert_eq!(
            error_bookends[0].model.as_deref(),
            Some("darkmux:qwen3-4b"),
            "the DROP-PATH error bookend must carry the namespaced identifier the map step \
             actually addresses, not the bare config.model: {:?}",
            error_bookends[0]
        );
    }

    #[test]
    fn compose_message_with_no_input_returns_base_unchanged() {
        let input = BTreeMap::new();
        assert_eq!(compose_message("hello", &input), "hello");
    }

    #[test]
    fn compose_message_prepends_dependency_outputs_in_key_order() {
        let mut input = BTreeMap::new();
        input.insert("b-step".to_string(), "B output".to_string());
        input.insert("a-step".to_string(), "A output".to_string());
        let composed = compose_message("base task", &input);
        let a_pos = composed.find("A output").unwrap();
        let b_pos = composed.find("B output").unwrap();
        let base_pos = composed.find("base task").unwrap();
        assert!(a_pos < b_pos, "a-step sorts before b-step (BTreeMap key order)");
        assert!(b_pos < base_pos, "dependency output precedes the base message");
    }

    #[test]
    fn parse_failed_verifiers_extracts_from_envelope() {
        let stdout = r#"{"failed_tool_invocations":[{"command":"cargo test","reason":"not found"}]}"#;
        let out = parse_failed_verifiers(stdout);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].command, "cargo test");
        assert_eq!(out[0].reason, "not found");
    }

    #[test]
    fn parse_failed_verifiers_empty_on_no_field() {
        let stdout = r#"{"status":"ok"}"#;
        assert!(parse_failed_verifiers(stdout).is_empty());
    }

    #[test]
    fn parse_failed_verifiers_empty_on_garbage() {
        assert!(parse_failed_verifiers("not json at all").is_empty());
    }

    /// (#1509) `RawDispatchOutcome`'s wire shape is a cross-module contract —
    /// `DispatchInternalStepKind::run` (this module) packs it,
    /// `dispatch_as_crew_of_one` (a different crate module) unpacks it. Lock
    /// the round-trip here so a field rename/retype on either side fails
    /// loud in THIS crate, not silently at the `serde_json::from_str` call
    /// in `dispatch_as_crew_of_one_with`.
    #[test]
    fn raw_dispatch_outcome_round_trips_through_json() {
        let original = RawDispatchOutcome {
            exit_code: 2,
            stdout: "some output".to_string(),
            stderr: "some warning".to_string(),
            session_id: crate::test_session("123-0"),
            execution_id: None,
            out_dir: Some(std::path::PathBuf::from("/tmp/darkmux-out")),
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: RawDispatchOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back.exit_code, 2);
        assert_eq!(back.stdout, "some output");
        assert_eq!(back.stderr, "some warning");
        assert_eq!(back.session_id, crate::test_session("123-0"));
        assert_eq!(back.out_dir, Some(std::path::PathBuf::from("/tmp/darkmux-out")));
    }

    #[test]
    fn raw_dispatch_outcome_out_dir_omitted_when_none() {
        let original = RawDispatchOutcome {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            session_id: crate::test_session("s"),
            execution_id: None,
            out_dir: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(!json.contains("out_dir"), "None out_dir must not serialize a key: {json}");
        let back: RawDispatchOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back.out_dir, None);
    }


    #[test]
    fn parse_failed_verifiers_falls_back_to_last_line() {
        let stdout = "some leading log noise\n{\"failed_tool_invocations\":[{\"command\":\"pytest\",\"reason\":\"toolchain missing\"}]}";
        let out = parse_failed_verifiers(stdout);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].command, "pytest");
    }

    #[test]
    fn dispatch_internal_requires_role_id() {
        let s = step("s1", "dispatch.internal", json!({"message": "hi"}));
        let err = DispatchInternalStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err();
        assert!(err.to_string().contains("config.role_id"), "{err}");
    }

    #[test]
    fn dispatch_single_shot_requires_model() {
        let s = step("s1", "dispatch.single_shot", json!({"user": "hi"}));
        let err = DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err();
        assert!(err.to_string().contains("missing required key `config.model`"), "{err}");
    }

    #[test]
    fn procedural_shell_requires_command() {
        let s = step("s1", "procedural.shell", json!({}));
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err();
        assert!(err.to_string().contains("missing required key `config.command`"), "{err}");
    }

    #[test]
    // (#2532) `#[serial_test::serial]`: this step names no `cwd`/`workdir`,
    // so it READS the process cwd through `resolve_shell_cwd`'s ambient tier.
    // See `CwdGuard`'s doc — a reader participates in that global too.
    #[serial_test::serial]
    fn procedural_shell_runs_and_captures_stdout() {
        let s = step("s1", "procedural.shell", json!({"command": "echo hello-shell"}));
        let out = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        assert!(out.output.contains("hello-shell"));
    }

    #[test]
    // (#2532) `#[serial_test::serial]`: this step names no `cwd`/`workdir`,
    // so it READS the process cwd through `resolve_shell_cwd`'s ambient tier.
    // See `CwdGuard`'s doc — a reader participates in that global too.
    #[serial_test::serial]
    fn procedural_shell_nonzero_exit_is_an_error() {
        let s = step("s1", "procedural.shell", json!({"command": "exit 3"}));
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err();
        assert!(err.to_string().contains("exited with"), "{err}");
    }

    /// (#2361, swarm S4-4) A shell step's command is BOUNDED. Before the
    /// fix this was a plain `.output()`: the step never returned, the
    /// mission stayed Active, and a caught SIGTERM/SIGINT could not reach
    /// the child because its pid was never registered. Unlike `mods.gate`
    /// (where a hung `test_command` is data about the RUN, recorded as a
    /// gate skip), a `procedural.shell` step that outran its bound has no
    /// output to hand on — it is a step ERROR naming the bound.
    #[test]
    // scopes DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS — AND (#2532) this step
    // names no `cwd`/`workdir`, so it reads the process cwd through
    // `resolve_shell_cwd`'s ambient tier. See `CwdGuard`'s doc.
    #[serial_test::serial]
    fn procedural_shell_past_the_deadline_is_killed_and_errors_with_the_bound() {
        let k = "DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS";
        let prev = std::env::var(k).ok();
        unsafe { std::env::set_var(k, "2") };
        let s = step("s1", "procedural.shell", json!({"command": "sleep 300"}));
        let started = std::time::Instant::now();
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err();
        let elapsed = started.elapsed();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        assert!(elapsed < std::time::Duration::from_secs(20), "must return at its bound, took {elapsed:?}");
        assert!(err.to_string().contains("exceeded 2s"), "{err}");
    }

    #[test]
    // (#2532) `#[serial_test::serial]`: this step names no `cwd`/`workdir`,
    // so it READS the process cwd through `resolve_shell_cwd`'s ambient tier.
    // See `CwdGuard`'s doc — a reader participates in that global too.
    #[serial_test::serial]
    fn procedural_shell_exposes_dependency_output_as_env_var() {
        let mut input = BTreeMap::new();
        input.insert("upstream-step".to_string(), "value-from-upstream".to_string());
        let s = step(
            "s1",
            "procedural.shell",
            json!({"command": "echo $DARKMUX_STEP_INPUT_UPSTREAM_STEP"}),
        );
        let out = ProceduralShellStepKind.run(&s, &empty_task(), &input, &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        assert!(out.output.contains("value-from-upstream"), "got: {}", out.output);
    }

    /// (#2310 P4e) A step command that has to call darkmux back gets the
    /// RUNNING binary's absolute path, not a `PATH` lookup. Red-proved by
    /// deleting the `cmd.env("DARKMUX_BIN", …)` line: the command then
    /// prints the empty string and the `is_absolute` assertion fails.
    #[test]
    // (#2532) `#[serial_test::serial]`: this step names no `cwd`/`workdir`,
    // so it READS the process cwd through `resolve_shell_cwd`'s ambient tier.
    // See `CwdGuard`'s doc — a reader participates in that global too.
    #[serial_test::serial]
    fn procedural_shell_exports_the_running_darkmux_binarys_path() {
        let s = step("s1", "procedural.shell", json!({"command": "printf %s \"${DARKMUX_BIN:-}\""}));
        let out = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        let seen = std::path::PathBuf::from(out.output.trim());
        assert!(
            seen.is_absolute() && seen.exists(),
            "DARKMUX_BIN must name the running binary by absolute path, got {:?}",
            out.output
        );
        assert_eq!(
            seen,
            std::env::current_exe().unwrap(),
            "and it must be THIS process's binary, never a PATH lookup"
        );
    }

    /// RAII guard that changes the process cwd for the test's duration and
    /// restores it on drop. (Same pattern as `mission_config::load`'s
    /// `CwdGuard`; not shared across modules because it is test-only and
    /// this module has its own test mod.)
    ///
    /// **Every test that WRITES the process cwd must be
    /// `#[serial_test::serial]` — and so must every test that READS it.**
    /// Reading a process-global is as much a participation as writing it,
    /// and `serial_test` excludes only other ANNOTATED tests (see
    /// `host_sampler_lock.rs`'s own note saying exactly this), so an
    /// unannotated reader runs concurrently with this guard by default.
    /// This guard is the sharpest case in the crate: one of its callers
    /// DELETES the directory while standing in it, so for that window
    /// `getcwd()` fails process-wide and any concurrent test reading it
    /// sees a "no longer exists" refusal in a shell step that has nothing
    /// to do with cwd — a product-bug-shaped intermittent.
    ///
    /// The readers, found by mutation (panic in `resolve_shell_cwd`'s
    /// ambient branch, then run `-p darkmux-crew`; the failures ARE the
    /// list) rather than by grep, and all annotated:
    /// `procedural_shell_runs_and_captures_stdout`,
    /// `procedural_shell_nonzero_exit_is_an_error`,
    /// `procedural_shell_exposes_dependency_output_as_env_var`,
    /// `procedural_shell_past_the_deadline_is_killed_and_errors_with_the_bound`,
    /// `procedural_shell_exports_the_running_darkmux_binarys_path`,
    /// `procedural_shell_valid_ambient_cwd_still_works_with_no_config`, and
    /// `scheduler::tests::dispatch_free_siblings_do_not_serialize_behind_a_serial_endpoint`
    /// (which runs shell steps on scheduler worker THREADS — same process,
    /// same global). Re-run that mutation after adding any
    /// `procedural.shell` test with no `cwd`/`workdir`.
    struct CwdGuard {
        prev: std::path::PathBuf,
    }

    impl CwdGuard {
        fn enter(dir: &std::path::Path) -> Self {
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir).unwrap();
            Self { prev }
        }
    }

    impl Drop for CwdGuard {
        /// A FAILED restore is fatal, not ignored: the process would be left
        /// standing in a deleted directory and every later test in this
        /// binary that reads cwd would fail for a reason that has nothing to
        /// do with it. `std::thread::panicking()` keeps a genuine test
        /// failure's own message intact instead of aborting the process on a
        /// double panic.
        fn drop(&mut self) {
            if let Err(e) = std::env::set_current_dir(&self.prev) {
                let msg = format!("CwdGuard could not restore the process cwd to {}: {e}", self.prev.display());
                if std::thread::panicking() {
                    eprintln!("{msg}");
                } else {
                    panic!("{msg}");
                }
            }
        }
    }

    /// (#2532) THE regression test. A step with no `cwd`/`workdir` and a
    /// Task with no `workdir` used to inherit the process's own ambient
    /// directory unconditionally — fine when that directory still exists,
    /// but the whole point of this repo's worktree workflow is that it
    /// routinely does not. Measured before the fix (see this test's sibling
    /// assertions in the PR description): a bare command still exited 0
    /// with `shell-init: error retrieving current directory` on stderr,
    /// `git status` exited 128, and a relative `ls` exited 1 — three
    /// different not-quite-clean outcomes for one config problem. This
    /// asserts the step now refuses loudly instead, BEFORE `sh` ever
    /// starts, naming the reason.
    #[test]
    #[serial_test::serial]
    fn procedural_shell_removed_ambient_cwd_is_refused_not_inherited() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = CwdGuard::enter(dir.path());
        std::fs::remove_dir(dir.path()).unwrap();
        // getcwd() now fails (ENOENT) even though nothing has chdir'd away.

        let s = step("s1", "procedural.shell", json!({"command": "echo should-not-run"}));
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err();
        assert!(
            err.to_string().contains("no longer exists"),
            "expected a refusal naming the missing directory, got: {err}"
        );
    }

    /// A step with no configured `cwd`/`workdir` and no task workdir keeps
    /// working exactly as before when the ambient directory is valid — the
    /// fix must not turn every ordinary context-free command (`date`,
    /// `curl`, `sysctl`) into a required-`cwd` step.
    #[test]
    // (#2532) Reads the process cwd by construction — that IS the tier under
    // test — so it must be serialized against the guard that deletes it.
    #[serial_test::serial]
    fn procedural_shell_valid_ambient_cwd_still_works_with_no_config() {
        let s = step("s1", "procedural.shell", json!({"command": "echo still-fine"}));
        let out = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        assert!(out.output.contains("still-fine"));
    }

    /// (#2532) `templates/builtin/mission-configs/review.json`'s
    /// `create-mod` task grows `"workdir": "{{item.tree_root}}"` into every
    /// step's config (`grow.config`'s merge), including its
    /// `wait-for-mod-step` — never `"cwd"`. Before this fix
    /// `procedural.shell` only ever read the `cwd` key, so that grown value
    /// was silently ignored. This proves the step-config `workdir` key (the
    /// shared spelling `dispatch_opts_for` reads) now actually sets the
    /// shell's cwd.
    ///
    /// **What this does NOT claim** (#2532 review, retracted): that
    /// honoring the key fixes that shipped step. Read its command — it runs
    /// `"${DARKMUX_BIN:-darkmux}" mod list --for "$key"`, an ABSOLUTE
    /// binary path from `current_exe()` against a store under
    /// `~/.darkmux`. It has no working-directory dependency at all, so
    /// nothing observable there changes. What DOES change for it is the
    /// other half of this fix: a `tree_root` that no longer exists now
    /// hard-errors a step that previously ignored it.
    #[serial_test::serial]
    #[test]
    fn procedural_shell_step_config_workdir_key_sets_the_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let s = step(
            "s1",
            "procedural.shell",
            json!({"command": "pwd", "workdir": dir.path().to_str().unwrap()}),
        );
        let out = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        assert_eq!(
            std::fs::canonicalize(out.output.trim()).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    /// (#3074) A step's `env` reaches the command as environment variables,
    /// never as shell text: a value full of quotes and `;` is read back
    /// byte for byte, and the command it would have broken out into does not
    /// run.
    #[serial_test::serial]
    #[test]
    fn procedural_shell_env_values_reach_the_command_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("injected");
        let evil = format!("0';touch {};#", marker.display());
        let s = step(
            "s1",
            "procedural.shell",
            json!({
                "command": "printf '%s' \"$DARKMUX_TEST_VALUE\"",
                "env": {"DARKMUX_TEST_VALUE": evil},
            }),
        );
        let out = ProceduralShellStepKind
            .run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test())
            .unwrap();
        assert_eq!(out.output, evil);
        assert!(!marker.exists(), "the value was run as shell");
    }

    /// The owning Task's `workdir` (e.g. a coder-phase worktree — "the
    /// mission's own tree root") is honored when the step itself names no
    /// `cwd`/`workdir`, matching the same fallback `dispatch.internal`
    /// already uses (`dispatch_opts_for`'s `task.workdir.clone().or_else(...)`).
    #[serial_test::serial]
    #[test]
    fn procedural_shell_task_workdir_is_the_default_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mut task = empty_task();
        task.workdir = Some(dir.path().to_path_buf());
        let s = step("s1", "procedural.shell", json!({"command": "pwd"}));
        let out = ProceduralShellStepKind.run(&s, &task, &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        assert_eq!(
            std::fs::canonicalize(out.output.trim()).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    /// (#2532) **The tier order between the two spellings of the SHARED
    /// `workdir` key, pinned.** `workdir` means one thing across kinds:
    /// `dispatch_opts_for` resolves it `task.workdir.clone().or_else(||
    /// cfg.workdir)` — Task first — and `StepKind`'s own
    /// doc states the same contract. Before this test, hoisting either
    /// branch above the other built clean and left the whole crate green,
    /// so nothing pinned it in either direction. Red-proved by swapping the
    /// `task.workdir` and `cfg.workdir` branches in
    /// `resolve_shell_cwd`: this test then reports the step's directory.
    #[serial_test::serial]
    #[test]
    fn procedural_shell_task_workdir_outranks_a_step_config_workdir() {
        let task_dir = tempfile::tempdir().unwrap();
        let step_dir = tempfile::tempdir().unwrap();
        let mut task = empty_task();
        task.workdir = Some(task_dir.path().to_path_buf());
        let s = step(
            "s1",
            "procedural.shell",
            json!({"command": "pwd", "workdir": step_dir.path().to_str().unwrap()}),
        );
        let out = ProceduralShellStepKind.run(&s, &task, &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        assert_eq!(
            std::fs::canonicalize(out.output.trim()).unwrap(),
            std::fs::canonicalize(task_dir.path()).unwrap(),
            "the Task's `workdir` outranks a step config `workdir`, matching `dispatch_opts_for`"
        );
    }

    /// (#2532) …and this kind's OWN `cwd` key outranks BOTH, which is the
    /// one place it deliberately differs from the shared vocabulary: there
    /// is no `Task.cwd`, so nothing can outrank `cwd`, and its pre-#2532
    /// meaning ("an explicit `cwd` on the step is where the command runs")
    /// is preserved exactly. `src/acp_panel.rs`'s `apply_default_cwd`
    /// depends on this — it injects the panel session's directory as `cwd`.
    #[serial_test::serial]
    #[test]
    fn procedural_shell_step_cwd_outranks_the_task_workdir() {
        let task_dir = tempfile::tempdir().unwrap();
        let step_dir = tempfile::tempdir().unwrap();
        let mut task = empty_task();
        task.workdir = Some(task_dir.path().to_path_buf());
        let s = step(
            "s1",
            "procedural.shell",
            json!({"command": "pwd", "cwd": step_dir.path().to_str().unwrap()}),
        );
        let out = ProceduralShellStepKind.run(&s, &task, &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        assert_eq!(
            std::fs::canonicalize(out.output.trim()).unwrap(),
            std::fs::canonicalize(step_dir.path()).unwrap()
        );
    }

    /// An explicit `cwd` (or `workdir`) that itself names a directory which
    /// does not exist is refused loudly at config-resolution time, rather
    /// than handed to `sh` to fail on its own — the same "refuse loudly"
    /// posture as the no-config case above, applied to a plain typo'd path.
    #[test]
    fn procedural_shell_explicit_cwd_that_does_not_exist_is_refused() {
        let s = step(
            "s1",
            "procedural.shell",
            json!({"command": "echo hi", "cwd": "/definitely/not/a/real/darkmux/path/2532"}),
        );
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("does not exist"), "{rendered}");
        assert!(rendered.contains("step config `cwd`"), "the refusal must name WHICH tier: {rendered}");
    }

    /// (#2532 review) A configured working directory goes through the
    /// SHARED validator (`darkmux_types::workdir::validate_workdir`), not a
    /// bare `is_dir()`. `is_dir()` FOLLOWS symlinks, so before this the step
    /// would happily run `sh -c` inside a link target the operator never
    /// named — the exact case that validator exists to refuse (#227/#2302).
    /// Red-proved by restoring the `path.is_dir()` check: the step then
    /// succeeds and prints the target's path.
    #[test]
    fn procedural_shell_cwd_through_a_symlink_is_refused_by_the_shared_validator() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let s = step("s1", "procedural.shell", json!({"command": "pwd", "cwd": link.to_str().unwrap()}));
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("symlink"),
            "the shared validator's symlink refusal must be what surfaces: {rendered}"
        );
    }

    /// (#2532 review) An EMPTY `cwd` gets its own message. Routed through
    /// the validator it renders as ``workdir path does not exist: `` —
    /// loud, but the empty backticks read as a darkmux bug rather than the
    /// unfilled config value (a `grow.config` placeholder that resolved to
    /// nothing) it almost always is.
    #[test]
    fn procedural_shell_empty_cwd_says_the_key_is_empty() {
        let s = step("s1", "procedural.shell", json!({"command": "echo hi", "cwd": ""}));
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("is set but empty"), "{rendered}");
    }

    #[test]
    fn procedural_noop_defaults_output_to_step_id() {
        let s = step("marker-step", "procedural.noop", json!(null));
        let out = ProceduralNoopStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        assert_eq!(out.output, "marker-step");
    }

    #[test]
    fn procedural_noop_honors_config_output_override() {
        let s = step("s1", "procedural.noop", json!({"output": "custom"}));
        let out = ProceduralNoopStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        assert_eq!(out.output, "custom");
    }

    // ── (#1402) Tier 1 display names ────────────────────────────────────

    #[test]
    fn tier1_display_names_match_the_spec() {
        assert_eq!(DispatchInternalStepKind.display_name(), "Dispatch");
        assert_eq!(DispatchSingleShotStepKind.display_name(), "Dispatch (single-shot)");
        assert_eq!(DispatchMapStepKind.display_name(), "Dispatch (map)");
        assert_eq!(ProceduralShellStepKind.display_name(), "Shell");
        assert_eq!(ProceduralNoopStepKind.display_name(), "No-op");
    }

    // ── (#1442) dispatch.map — the generic per-item map block ────────────

    fn map_step(config: serde_json::Value) -> Step {
        step("m1", "dispatch.map", config)
    }

    #[test]
    fn dispatch_map_requires_model() {
        // A non-empty collection reaches the model check; an empty one
        // short-circuits BEFORE it (tested separately), so give one item.
        let s = map_step(json!({ "user_template": "check {item}", "collection": ["a"] }));
        let err = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err();
        assert!(err.to_string().contains("missing required key `config.model`"), "{err}");
    }

    #[test]
    fn dispatch_map_requires_user_template_once_the_collection_is_non_empty() {
        let s = map_step(json!({ "model": "m", "collection": ["a"] }));
        let err = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err();
        assert!(err.to_string().contains("missing required key `config.user_template`"), "{err}");
    }

    #[test]
    fn map_item_text_substitutes_strings_verbatim_and_json_compactly() {
        assert_eq!(map_item_text(&json!("hello")), "hello");
        assert_eq!(map_item_text(&json!({ "id": "b1" })), r#"{"id":"b1"}"#);
        assert_eq!(map_item_text(&json!(42)), "42");
    }

    /// (#2310 P1 review finding I1) A `{system, item}` object is the ONLY
    /// shape that overrides the step's `system` — every other item shape
    /// (a bare string, a plain object, an object carrying only ONE of the
    /// two keys) is unaffected and is substituted whole via
    /// [`map_item_text`], exactly as before this field existed.
    #[test]
    fn item_system_and_payload_only_overrides_for_the_full_system_and_item_shape() {
        let full = json!({ "system": "OVERRIDE", "item": "hello" });
        let (system, payload) = item_system_and_payload(&full);
        assert_eq!(system, Some("OVERRIDE"));
        assert_eq!(payload, &json!("hello"));

        let plain_string = json!("plain string");
        let (system, payload) = item_system_and_payload(&plain_string);
        assert_eq!(system, None);
        assert_eq!(payload, &plain_string);

        let unrelated_object = json!({ "id": "b1" });
        let (system, payload) = item_system_and_payload(&unrelated_object);
        assert_eq!(system, None, "an unrelated object is not mistaken for the override shape");
        assert_eq!(payload, &unrelated_object);

        let system_only = json!({ "system": "lonely" });
        let (system, payload) = item_system_and_payload(&system_only);
        assert_eq!(system, None, "a \"system\" key with no \"item\" sibling is not an override");
        assert_eq!(payload, &system_only);

        let extra_key = json!({ "system": "OVERRIDE", "item": "hello", "id": "b1" });
        let (system, payload) = item_system_and_payload(&extra_key);
        assert_eq!(system, None, "an object with the two keys PLUS extras is a plain item, not an override");
        assert_eq!(payload, &extra_key);

        let non_string_system = json!({ "system": 5, "item": "hello" });
        let (system, payload) = item_system_and_payload(&non_string_system);
        assert_eq!(system, None, "a non-string system is not an override, and the payload is NOT swapped either");
        assert_eq!(payload, &non_string_system);

        let item_only = json!({ "item": "lonely" });
        let (system, payload) = item_system_and_payload(&item_only);
        assert_eq!(system, None, "an \"item\" key with no \"system\" sibling is not an override");
        assert_eq!(payload, &item_only);
    }

    #[test]
    fn resolve_map_collection_prefers_config_collection() {
        let s = map_step(json!({ "collection": ["a", "b", "c"] }));
        let out = resolve_map_collection(&s, &empty_task(), &BTreeMap::new()).unwrap();
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn resolve_map_collection_reads_the_single_dependency_input_as_a_json_array() {
        let s = map_step(json!({}));
        let mut input = BTreeMap::new();
        input.insert("upstream".to_string(), r#"["x","y"]"#.to_string());
        let out = resolve_map_collection(&s, &empty_task(), &input).unwrap();
        assert_eq!(out, vec![json!("x"), json!("y")]);
    }

    #[test]
    fn resolve_map_collection_reads_the_named_collection_input() {
        let s = map_step(json!({ "collection_input": "bundles" }));
        let mut input = BTreeMap::new();
        input.insert("bundles".to_string(), r#"["only-this"]"#.to_string());
        input.insert("other".to_string(), r#"["ignored"]"#.to_string());
        let out = resolve_map_collection(&s, &empty_task(), &input).unwrap();
        assert_eq!(out, vec![json!("only-this")]);
    }

    #[test]
    fn resolve_map_collection_truly_absent_or_blank_source_is_an_empty_collection() {
        // No config.collection, no collection_input, ZERO inputs — the one
        // genuinely collection-less shape that stays a real, silent zero.
        let s = map_step(json!({}));
        assert!(resolve_map_collection(&s, &empty_task(), &BTreeMap::new()).unwrap().is_empty());
        // A present-but-blank input string is a real zero too (the upstream
        // truly produced nothing), not an error.
        let mut input = BTreeMap::new();
        input.insert("u".to_string(), "   ".to_string());
        assert!(resolve_map_collection(&s, &empty_task(), &input).unwrap().is_empty());
    }

    /// A `Task` whose step ids are exactly `ids`, for the position-aware
    /// `resolve_map_collection` tests below — unlike `empty_task()` (whose
    /// single `step_ids` entry never matches a `map_step`'s `"m1"` id), this
    /// lets a test control whether the map step is the task's FIRST step or
    /// a later one.
    fn task_with_step_ids(ids: &[&str]) -> Task {
        let mut t = empty_task();
        t.step_ids = ids.iter().map(|s| s.to_string()).collect();
        t
    }

    #[test]
    fn resolve_map_collection_non_first_step_prefers_predecessor_over_ambiguity() {
        // (#2310 P2a) `gather_inputs` now chains a Task's `depends_on`/
        // `reads` outputs onto EVERY step's input, so a non-first
        // `dispatch.map` step (like `review.json`'s four probe/verify map
        // steps, which stamp no `collection_input`) can legitimately see
        // two or more inputs now, even though there is still only one
        // sensible collection source: its predecessor. Before #2310 P2a
        // this same input shape would hit the "two or more dependency
        // inputs... name which input carries the collection" bail — RED
        // without the predecessor-preference fix.
        let task = task_with_step_ids(&["render-step", "m1"]);
        let s = map_step(json!({}));
        let mut input = BTreeMap::new();
        input.insert("render-step".to_string(), r#"["from-predecessor"]"#.to_string());
        input.insert("some-read-task".to_string(), r#"["from-a-task-level-read"]"#.to_string());
        let out = resolve_map_collection(&s, &task, &input).unwrap();
        assert_eq!(
            out,
            vec![json!("from-predecessor")],
            "a non-first map step must resolve its collection from its predecessor, \
             not bail on the extra `reads`-sourced input"
        );
    }

    /// (#2341 review) An explicit `collection_input` on a NON-first step still
    /// wins over the predecessor preference — the operator's word beats the
    /// default.
    #[test]
    fn resolve_map_collection_non_first_step_explicit_collection_input_beats_predecessor() {
        let task = task_with_step_ids(&["render-step", "m1"]);
        let s = map_step(json!({ "collection_input": "some-read-task" }));
        let mut input = BTreeMap::new();
        input.insert("render-step".to_string(), r#"["from-predecessor"]"#.to_string());
        input.insert("some-read-task".to_string(), r#"["from-explicit"]"#.to_string());
        let out = resolve_map_collection(&s, &task, &input).unwrap();
        assert_eq!(out, vec![json!("from-explicit")]);
    }

    #[test]
    fn resolve_map_collection_first_step_two_inputs_still_bails_loud() {
        // (#2310 P2a) The predecessor preference applies ONLY to a non-first
        // step — a first step has no predecessor to prefer, so two
        // dependency inputs with no `collection_input` must still bail
        // exactly as loudly as before this change.
        let task = task_with_step_ids(&["m1", "later-step"]);
        let s = map_step(json!({}));
        let mut two = BTreeMap::new();
        two.insert("a".to_string(), r#"["x"]"#.to_string());
        two.insert("b".to_string(), r#"["y"]"#.to_string());
        let err = resolve_map_collection(&s, &task, &two).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("two or more dependency inputs"), "{msg}");
        assert!(msg.contains("collection_input"), "the fix is named: {msg}");
    }

    #[test]
    fn resolve_map_collection_two_inputs_without_collection_input_is_a_loud_error() {
        // (#1442 gate MUST FIX iii) Two or more dependency inputs and no
        // collection_input bails — resolving to empty would not be a refusal
        // to guess, it would BE a guess ("there is no collection").
        let s = map_step(json!({}));
        let mut two = BTreeMap::new();
        two.insert("a".to_string(), r#"["x"]"#.to_string());
        two.insert("b".to_string(), r#"["y"]"#.to_string());
        let err = resolve_map_collection(&s, &empty_task(), &two).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("two or more dependency inputs"), "{msg}");
        assert!(msg.contains("collection_input"), "the fix is named: {msg}");
        assert!(msg.contains("a, b"), "the ambiguous inputs are listed: {msg}");
    }

    #[test]
    fn resolve_map_collection_missing_named_input_key_is_a_loud_error() {
        // (#1442 gate MUST FIX i) A collection_input naming a key absent from
        // the gathered inputs is a typo-shaped config error — bail naming the
        // missing key AND the inputs actually present, never a silent empty.
        let s = map_step(json!({ "collection_input": "bundles" }));
        let mut input = BTreeMap::new();
        input.insert("upstream".to_string(), r#"["x"]"#.to_string());
        let err = resolve_map_collection(&s, &empty_task(), &input).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("`bundles`"), "the missing key is named: {msg}");
        assert!(msg.contains("upstream"), "the present inputs are named: {msg}");

        // Zero inputs at all: same loud error, with "none" as the roster.
        let err = resolve_map_collection(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("none"), "{err}");
    }

    #[test]
    fn resolve_map_collection_non_array_config_collection_is_a_loud_error() {
        // (#1442 gate MUST FIX ii) A present-but-non-array config.collection
        // bails, matching the input-side non-array error's loudness — it must
        // never silently fall through to input resolution.
        let s = map_step(json!({ "collection": "not-an-array" }));
        let mut input = BTreeMap::new();
        input.insert("u".to_string(), r#"["would-be-used-on-fallthrough"]"#.to_string());
        let err = resolve_map_collection(&s, &empty_task(), &input).unwrap_err();
        assert!(err.to_string().contains("`config.collection` must be a list"), "{err}");
    }

    #[test]
    fn resolve_map_collection_non_array_source_is_a_loud_error() {
        let s = map_step(json!({}));
        let mut input = BTreeMap::new();
        input.insert("u".to_string(), r#"{"not":"an array"}"#.to_string());
        let err = resolve_map_collection(&s, &empty_task(), &input).unwrap_err();
        assert!(err.to_string().contains("must be a JSON array"), "{err}");
    }

    #[test]
    fn dispatch_map_empty_collection_short_circuits_without_dispatch() {
        // The block-level short-circuit (#1442): an empty collection returns
        // an empty `[]` output with a named short-circuit record, and NEVER
        // reaches the model/user_template requirements or any dispatch — so a
        // config missing `model` still succeeds here (nothing to dispatch).
        let s = map_step(json!({ "collection": [] }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        assert_eq!(out.output, "[]");
        assert_eq!(out.flow_records.len(), 1, "one short-circuit record");
        let payload = out.flow_records[0].payload_json();
        assert!(payload["short_circuit"].as_str().unwrap().contains("empty collection"));
    }

    /// (#2394) The four `dispatch.map` seat outcomes, each asserted as its
    /// OWN class. These four tests all used to assert `seat().is_none()`
    /// — the same assertion for an empty collection, a hosted endpoint, and a
    /// missing `n_ctx`, three facts with nothing in common. That shared
    /// `None` was the bug: whatever the scheduler did with it, it did to all
    /// three.
    #[test]
    fn dispatch_map_empty_collection_claims_no_model_so_nothing_loads() {
        // The no-load property: even with a full local residency config
        // (model + n_ctx present), an EMPTY collection is a guaranteed no-op,
        // so the wave loader is never asked to load a model the map won't
        // use. This is the #1442 empty-docket short-circuit, generic.
        let s = map_step(json!({
            "model": "some-local-model",
            "user_template": "check {item}",
            "n_ctx": 8192,
            "collection": [],
        }));
        assert!(
            matches!(
                DispatchMapStepKind.seat(&s, &empty_task(), &BTreeMap::new(), &bare_ctx()),
                SeatClaim::NoModel
            ),
            "an empty collection consumes no model at all — not a remote seat, not an \
             unresolved local one"
        );
    }

    #[test]
    fn dispatch_map_local_seat_resolves_a_placement_for_a_non_empty_collection() {
        let s = map_step(json!({
            "model": "qwen3.6-35b-a3b",
            "user_template": "check {item}",
            "n_ctx": 8192,
            "collection": ["a"],
        }));
        let SeatClaim::LocalModel(placement) =
            DispatchMapStepKind.seat(&s, &empty_task(), &BTreeMap::new(), &bare_ctx())
        else {
            panic!("a local model with full residency hints must claim LocalModel");
        };
        assert_eq!(placement.model_key, "qwen3.6-35b-a3b");
        assert_eq!(placement.min_ctx, 8192);
        assert!(placement.identifier.starts_with("darkmux:"), "default identifier is namespaced: {}", placement.identifier);
        // (#1442 gate C7) "step:<id>", consistent with dispatch.internal's
        // placement provenance.
        assert_eq!(placement.seat, "step:m1");
    }

    #[test]
    fn dispatch_map_hosted_claims_an_unmanaged_endpoint() {
        // A map on an unmanaged endpoint loads nothing locally and claims the
        // endpoint, which `limits.concurrent_calls` bounds.
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "n_ctx": 8192,
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
        }));
        assert!(matches!(
            DispatchMapStepKind.seat(&s, &empty_task(), &BTreeMap::new(), &bare_ctx()),
            SeatClaim::UnmanagedEndpoint(_)
        ));
    }

    #[test]
    fn dispatch_map_without_n_ctx_claims_local_unresolved_and_names_why() {
        let s = map_step(json!({ "model": "m", "user_template": "u {item}", "collection": ["a"] }));
        let claim = DispatchMapStepKind.seat(&s, &empty_task(), &BTreeMap::new(), &bare_ctx());
        let SeatClaim::LocalModelUnresolved { reason } = claim else {
            panic!("a local seat missing its n_ctx hint is UNRESOLVED, never silently remote");
        };
        assert!(reason.contains("n_ctx"), "the reason names the missing field: {reason}");
    }

    fn map_ep() -> darkmux_types::ModelEndpoint {
        darkmux_types::ModelEndpoint { url: Some("http://127.0.0.1:1".to_string()), ..Default::default() }
    }

    /// (#2902 step 5, #3035) A spent dispatch cap under `warn` never skips or
    /// clamps a map item: the call fires with the FULL requested cap, and
    /// the item is ok. (Pre-4.0 this item was skipped with a named reason,
    /// and a nearly-spent bucket clamped the cap.)
    #[test]
    #[serial_test::serial] // IsolatedState mutates process-global env
    fn a_spent_step_cap_under_warn_never_skips_or_clamps_an_item() {
        let _state = darkmux_types::test_isolation::IsolatedState::new(); // pins HOME/DARKMUX_HOME: the step budget's `budget.warn` goes to the real flow sink otherwise
        let bucket = Arc::new(Mutex::new(DispatchBudget::new(Some(1_000), darkmux_types::BudgetPolicy::Warn)));
        {
            let mut b = bucket.lock().unwrap();
                        b.settle(5_000);
            assert!(b.exhausted());
        }
        let sent = Arc::new(Mutex::new(Vec::<u32>::new()));
        let seen = Arc::clone(&sent);
        let ovr: MapDispatchOverride = Arc::new(move |call: &OverrideDispatchCall<'_>| {
            seen.lock().unwrap().push(call.max_tokens);
            Ok(crate::single_shot::SingleShotReply {
                content: "answer".into(),
                model: None,
                counts: darkmux_trajectory::UsageCounts { total: Some(700), ..Default::default() },
            })
        });
        let out = map_hosted_item(
            0, &bucket, &map_ep(), "gpt-5.1", "sys", "user", 4_096, 1, 0, 0, Some(&ovr),
            &mut Vec::new(), "s1", &crate::budget::tests::solo_caller(),
        );
        assert!(out.ok, "{out:?}");
        assert_eq!(out.content, "answer");
        assert_eq!(*sent.lock().unwrap(), vec![4_096], "fired once, with the full cap");
        assert_eq!(bucket.lock().unwrap().settled(), 5_700, "settled with the real spend");
    }

    /// The dispatch cap settles with the amount the call's usage record carries:
    /// a reply that reported a split and no total settles prompt +
    /// completion, as its record's `total_tokens` does, not the whole
    /// granted cap (which is only for a reply that reported nothing). The
    /// item's own total is the same number.
    #[test]
    fn a_map_call_settles_the_amount_its_usage_record_carries() {
        let bucket = Arc::new(Mutex::new(DispatchBudget::new(None, darkmux_types::BudgetPolicy::Warn)));
        let ovr: MapDispatchOverride = Arc::new(move |_call: &OverrideDispatchCall<'_>| {
            Ok(crate::single_shot::SingleShotReply {
                content: "answer".into(),
                model: None,
                counts: darkmux_trajectory::UsageCounts { prompt: Some(30), completion: Some(12), ..Default::default() },
            })
        });
        let mut calls = Vec::new();
        let out = map_hosted_item(
            0, &bucket, &map_ep(), "gpt-5.1", "sys", "user", 4_096, 0, 0, 0, Some(&ovr),
            &mut calls, "s1", &crate::budget::tests::solo_caller(),
        );
        let record = serde_json::to_value(map_call_token_payload(&calls[0], 0, "gpt-5.1", "ep", None)).unwrap();
        assert_eq!(record["total_tokens"], 42);
        assert_eq!(bucket.lock().unwrap().settled(), 42, "settled with the record's amount, not the 4096 cap");
        assert_eq!(out.total_tokens, Some(42));
    }

    /// A reply that reports its completion but not its prompt has an
    /// UNKNOWN spend (the prompt is usually most of it), so the call settles
    /// the whole granted cap plus the prompt it sent, the same as a reply
    /// that reported nothing, and its record carries no total.
    #[test]
    fn a_map_call_with_no_prompt_count_settles_the_cap_plus_its_prompt() {
        let bucket = Arc::new(Mutex::new(DispatchBudget::new(None, darkmux_types::BudgetPolicy::Warn)));
        let ovr: MapDispatchOverride = Arc::new(move |_call: &OverrideDispatchCall<'_>| {
            Ok(crate::single_shot::SingleShotReply {
                content: "answer".into(),
                model: None,
                counts: darkmux_trajectory::UsageCounts { completion: Some(12), ..Default::default() },
            })
        });
        let mut calls = Vec::new();
        let out = map_hosted_item(
            0, &bucket, &map_ep(), "gpt-5.1", "sys", "user", 4_096, 0, 0, 0, Some(&ovr),
            &mut calls, "s1", &crate::budget::tests::solo_caller(),
        );
        let record = serde_json::to_value(map_call_token_payload(&calls[0], 0, "gpt-5.1", "ep", None)).unwrap();
        assert!(record.get("total_tokens").is_none_or(|t| t.is_null()), "{record}");
        assert_eq!(record["completion_tokens"], 12, "the partial count is still recorded");
        // `max_tokens` bounds only the completion: the prompt the request
        // carried is charged too, by its estimate, never 0.
        let prompt = darkmux_trajectory::estimate_tokens("sys") + darkmux_trajectory::estimate_tokens("user");
        let used = bucket.lock().unwrap().settled();
        assert!(used >= 4_096 + prompt as u64, "spend unknown: the cap plus the prompt estimate, got {used}");
        assert_eq!(out.total_tokens, None);
    }

    /// (re-review N2) The conservative charge for an unknown spend is the
    /// granted completion cap PLUS the request's prompt, estimated from the
    /// request actually sent: a 40,000-char prompt is ~10,000 tokens the
    /// cap alone never bounded.
    #[test]
    fn an_unknown_spend_is_charged_the_cap_plus_the_prompt_it_sent() {
        let big = serde_json::json!({ "messages": [{ "role": "user", "content": "x".repeat(40_000) }], "max_tokens": 4096 });
        let charged = crate::budget::conservative_hosted_spend(None, 4096, &big);
        assert!(charged >= 4096 + 10_000, "{charged}");
        assert_eq!(charged, 4096 + darkmux_trajectory::estimate_tokens(&big.to_string()) as u64);
        assert_eq!(crate::budget::conservative_hosted_spend(Some(1234), 4096, &big), 1234, "a known spend is charged as reported");
    }

    /// (#1605 QA finding, kept) An item whose first dispatch errored and
    /// whose retry came back empty reports the ERROR, never a fabricated
    /// empty success.
    #[test]
    fn an_errored_then_empty_item_reports_the_error_not_a_fabricated_success() {
        let bucket = Arc::new(Mutex::new(DispatchBudget::new(None, darkmux_types::BudgetPolicy::Warn)));
        let calls = Arc::new(Mutex::new(0usize));
        let seen = Arc::clone(&calls);
        let ovr: MapDispatchOverride = Arc::new(move |_call: &OverrideDispatchCall<'_>| {
            let mut n = seen.lock().unwrap();
            *n += 1;
            if *n == 1 {
                anyhow::bail!("endpoint refused the draw")
            }
            Ok(crate::single_shot::SingleShotReply {
                content: String::new(),
                model: None,
                counts: darkmux_trajectory::UsageCounts { total: Some(1), ..Default::default() },
            })
        });
        let out = map_hosted_item(
            0, &bucket, &map_ep(), "gpt-5.1", "sys", "user", 1_000, 1, 0, 1, Some(&ovr),
            &mut Vec::new(), "s1", &crate::budget::tests::solo_caller(),
        );
        assert!(!out.ok, "{out:?}");
        assert!(out.error.as_deref().unwrap_or_default().contains("endpoint refused the draw"), "{out:?}");
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    /// (#3074) The LOCAL sibling of the test above: an item whose first
    /// dispatch errored and whose retry came back empty reports the error,
    /// not an empty success that dropped it.
    #[test]
    fn a_local_item_that_errored_then_came_back_empty_reports_the_error() {
        let calls = Arc::new(Mutex::new(0usize));
        let seen = Arc::clone(&calls);
        let ovr: MapDispatchOverride = Arc::new(move |_call: &OverrideDispatchCall<'_>| {
            let mut n = seen.lock().unwrap();
            *n += 1;
            if *n == 1 {
                anyhow::bail!("local endpoint refused the draw")
            }
            Ok(crate::single_shot::SingleShotReply {
                content: String::new(),
                model: None,
                counts: darkmux_trajectory::UsageCounts { total: Some(1), ..Default::default() },
            })
        });
        let out = map_local_item(3, "m", "sys", "user", 0.0, 100, 5, 0, 1, Some(&ovr), &mut Vec::new(), None);
        assert!(!out.ok, "{out:?}");
        assert!(out.error.as_deref().unwrap_or_default().contains("local endpoint refused the draw"), "{out:?}");
        assert_eq!(out.index, 3, "the result names its own item, which is how the step output and its usage records key it");
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    /// The inverted case: with no error anywhere, an all-empty item is still
    /// the deliberate `ok: true, content: ""` ("dispatched, produced nothing").
    #[test]
    fn a_local_item_that_only_came_back_empty_stays_an_ok_empty_item() {
        let ovr: MapDispatchOverride = Arc::new(|_call: &OverrideDispatchCall<'_>| {
            Ok(crate::single_shot::SingleShotReply {
                content: String::new(),
                model: None,
                counts: darkmux_trajectory::UsageCounts { total: Some(1), ..Default::default() },
            })
        });
        let out = map_local_item(3, "m", "sys", "user", 0.0, 100, 5, 0, 1, Some(&ovr), &mut Vec::new(), None);
        assert!(out.ok && out.error.is_none() && out.content.is_empty(), "{out:?}");
        assert_eq!(out.index, 3, "{out:?}");
    }

    /// A named endpoint with a 1-token daily budget under `policy`.
    fn budgeted_ep(policy: &str) -> darkmux_types::ModelEndpoint {
        let mut ep: darkmux_types::ModelEndpoint = serde_json::from_value(json!({
            "url": "http://127.0.0.1:1",
            "limits": { "policy": policy, "window": { "period": "1d", "tokens": 1 } },
        }))
        .unwrap();
        ep.source = darkmux_types::EndpointSource::Named("azure".into());
        ep
    }

    /// (#2902 step 5 review M1) An endpoint `wait` on an ABORTED mission
    /// ends without sending: the waiter reads the mission's terminal state
    /// from disk (what `darkmux mission abort` writes, from another
    /// process) and the transport is never called.
    #[test]
    #[serial_test::serial]
    fn a_map_item_waiting_on_an_aborted_missions_budget_never_sends() {
        let crew = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", crew.path()) };
        let mpath = crate::lifecycle::mission_path("m-aborted");
        std::fs::create_dir_all(mpath.parent().unwrap()).unwrap();
        std::fs::write(&mpath, r#"{"id":"m-aborted","status":"aborted"}"#).unwrap();
        let env = std::rc::Rc::new(crate::budget::tests::FakeEnv::full_window().reading_disk_for_stops());
        let calls = Arc::new(Mutex::new(0usize));
        let seen = Arc::clone(&calls);
        let ovr: MapDispatchOverride = Arc::new(move |_call: &OverrideDispatchCall<'_>| {
            *seen.lock().unwrap() += 1;
            anyhow::bail!("must not be called")
        });
        let bucket = Arc::new(Mutex::new(DispatchBudget::new(None, darkmux_types::BudgetPolicy::Warn)));
        let caller = crate::budget::tests::mission_caller("m-aborted");
        let out = crate::budget::with_test_env(env.clone(), || {
            map_hosted_item(
                0, &bucket, &budgeted_ep("wait"), "gpt-5.1", "sys", "user", 1_000, 1, 0, 0, Some(&ovr),
                &mut Vec::new(), "s1", &caller,
            )
        });
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert!(!out.ok);
        let err = out.error.unwrap_or_default();
        assert!(err.contains("mission `m-aborted` is aborted") && err.contains("nothing was sent"), "{err}");
        assert_eq!(*calls.lock().unwrap(), 0, "the transport is never called");
    }

    /// (#2925) One policy for a failed hosted call, on the map arm: a timeout
    /// after sending or a 5xx charges the conservative no-usage amount and is
    /// counted as a call; a failure before sending, a 4xx and a 429 spend
    /// nothing and are not counted.
    #[test]
    fn a_failed_hosted_map_call_charges_only_when_the_endpoint_may_have_processed_it() {
        use crate::dispatch_internal::{failure_for_test, HostedFailure};
        let run = |kind: HostedFailure| {
            let bucket = Arc::new(Mutex::new(DispatchBudget::new(None, darkmux_types::BudgetPolicy::Warn)));
            let ovr: MapDispatchOverride =
                Arc::new(move |_c: &OverrideDispatchCall<'_>| Err(failure_for_test(kind, "failed")));
            let mut calls = Vec::new();
            let out = map_hosted_item(
                0, &bucket, &map_ep(), "gpt-5.1", "sys", "user", 4_096, 0, 0, 0, Some(&ovr),
                &mut calls, "s1", &crate::budget::tests::solo_caller(),
            );
            assert!(!out.ok, "{out:?}");
            let settled = bucket.lock().unwrap().settled();
            (settled, calls.len())
        };
        let prompt = (darkmux_trajectory::estimate_tokens("sys") + darkmux_trajectory::estimate_tokens("user")) as u64;
        for kind in [HostedFailure::Unanswered, HostedFailure::ServerError] {
            let (settled, calls) = run(kind);
            assert!(settled >= 4_096 + prompt.min(1), "{kind:?} is charged the cap plus the prompt: {settled}");
            assert_eq!(calls, 1, "{kind:?} is counted as a call");
        }
        for kind in [HostedFailure::NotSent, HostedFailure::Rejected, HostedFailure::RateLimited] {
            assert_eq!(run(kind), (0, 0), "{kind:?} spends nothing and is not a call");
        }
    }

    /// The same policy on the hosted `dispatch.single_shot` step, end to end
    /// through the real curl path: a 500 breaches a one-token cap (charged),
    /// a 401 does not (spent nothing).
    #[test]
    #[serial_test::serial]
    fn a_failed_hosted_single_shot_step_charges_only_when_the_endpoint_may_have_processed_it() {
        let _state = darkmux_types::test_isolation::IsolatedState::new();
        let actions = |status: &'static str, body: &'static str| {
            let url = crate::budget::tests::status_http_mock(status, body);
            let base = url.trim_end_matches("/v1/chat/completions").to_string();
            let reg = tempfile::TempDir::new().unwrap();
            let pf = reg.path().join("profiles.json");
            std::fs::write(
                &pf,
                format!(
                    r#"{{"profiles":{{"p":{{"models":[{{"id":"m","n_ctx":1}}]}}}},
                        "endpoints":{{"azure":{{"url":"{base}","limits":{{"tokens_per_dispatch":1}}}}}}}}"#
                ),
            )
            .unwrap();
            let s = step(
                "s1",
                "dispatch.single_shot",
                json!({ "model": "gpt-5.1", "user": "hi", "endpoint": "azure", "config_path": pf.to_str().unwrap(), "timeout_seconds": 5 }),
            );
            let env = std::rc::Rc::new(crate::budget::tests::FakeEnv::new(vec![]));
            let err = crate::budget::with_test_env(env.clone(), || {
                DispatchSingleShotStepKind
                    .run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test())
                    .unwrap_err()
            });
            assert!(format!("{err:#}").contains("dispatch.single_shot (hosted)"), "{err:#}");
            env.actions()
        };
        assert_eq!(
            actions("500 Internal Server Error", r#"{"error":{"code":401,"message":"body says 401"}}"#),
            vec![darkmux_flow::FlowAction::BudgetWarn],
            "the 500 status decides, not the body's 401: charged against the cap"
        );
        assert!(
            actions("401 Unauthorized", r#"{"error":{"code":500,"message":"body says 500"}}"#).is_empty(),
            "the 401 status decides, not the body's 500: nothing charged"
        );
        assert_eq!(
            actions("502 Bad Gateway", "<html>502</html>"),
            vec![darkmux_flow::FlowAction::BudgetWarn],
            "an HTML 502 is a 5xx: charged"
        );
        assert!(actions("404 Not Found", "<html>404</html>").is_empty(), "an HTML 404 is a 4xx: nothing charged");
    }

    /// (5th review C2) A hosted map item settles its REPLY's spend into
    /// the step bucket: a 10-token cap and a 12-token reply warn with
    /// `spent: 12` (settling 0 would leave the cap silent).
    #[test]
    fn a_hosted_map_item_settles_its_reply_into_the_step_cap() {
        let env = std::rc::Rc::new(crate::budget::tests::FakeEnv::new(vec![]));
        let ovr: MapDispatchOverride = Arc::new(move |_call: &OverrideDispatchCall<'_>| {
            Ok(crate::single_shot::SingleShotReply {
                content: "ok".into(),
                model: None,
                counts: darkmux_trajectory::UsageCounts { total: Some(12), ..Default::default() },
            })
        });
        let bucket = Arc::new(Mutex::new(DispatchBudget::new(Some(10), darkmux_types::BudgetPolicy::Warn)));
        let ep: darkmux_types::ModelEndpoint = serde_json::from_value(json!({ "url": "https://h.example/v1" })).unwrap();
        let out = crate::budget::with_test_env(env.clone(), || {
            map_hosted_item(
                0, &bucket, &ep, "gpt-5.1", "sys", "user", 5, 1, 0, 0, Some(&ovr),
                &mut Vec::new(), "s1", &crate::budget::tests::solo_caller(),
            )
        });
        assert!(out.ok, "{out:?}");
        assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWarn]);
        let w = env.payload(darkmux_flow::FlowAction::BudgetWarn);
        assert_eq!((w["scope"].as_str(), w["spent"].as_u64(), w["limit"].as_u64()), (Some("dispatch"), Some(12), Some(10)), "{w}");
    }

    /// (#2902 step 5 review C1) The endpoint gate fires on the map path: a
    /// full window under `warn` warns through the gate before the call.
    #[test]
    fn the_endpoint_gate_fires_on_a_hosted_map_item() {
        let env = std::rc::Rc::new(crate::budget::tests::FakeEnv::full_window());
        let ovr: MapDispatchOverride = Arc::new(move |_call: &OverrideDispatchCall<'_>| {
            Ok(crate::single_shot::SingleShotReply {
                content: "ok".into(),
                model: None,
                counts: darkmux_trajectory::UsageCounts { total: Some(1), ..Default::default() },
            })
        });
        let bucket = Arc::new(Mutex::new(DispatchBudget::new(None, darkmux_types::BudgetPolicy::Warn)));
        let out = crate::budget::with_test_env(env.clone(), || {
            map_hosted_item(
                0, &bucket, &budgeted_ep("warn"), "gpt-5.1", "sys", "user", 1_000, 1, 0, 0, Some(&ovr),
                &mut Vec::new(), "s1", &crate::budget::tests::solo_caller(),
            )
        });
        assert!(out.ok, "{out:?}");
        assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWarn], "the gate fired");
    }

    /// (#2902 step 5 review C1) The endpoint gate fires on the hosted
    /// `dispatch.single_shot` arm, before its call: a full window under
    /// `wait` with the run stopped returns the gate's error and the step
    /// never reaches its transport (no hosted-call error context).
    #[test]
    fn the_endpoint_gate_fires_on_a_hosted_single_shot_step() {
        let reg = tempfile::TempDir::new().unwrap();
        let pf = reg.path().join("profiles.json");
        std::fs::write(
            &pf,
            r#"{"profiles":{"p":{"models":[{"id":"m","n_ctx":1}]}},
                "endpoints":{"azure":{"url":"http://127.0.0.1:1",
                    "limits":{"policy":"wait","window":{"period":"1d","tokens":1}}}}}"#,
        )
        .unwrap();
        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({ "model": "gpt-5.1", "user": "hi", "endpoint": "azure", "config_path": pf.to_str().unwrap(), "timeout_seconds": 1 }),
        );
        let env = std::rc::Rc::new(crate::budget::tests::FakeEnv::full_window().stopped_after(1, "mission `x` is aborted"));
        let msg = crate::budget::with_test_env(env.clone(), || {
            format!("{:#}", DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err())
        });
        assert!(msg.contains("stopped waiting on endpoint `azure`'s budget"), "{msg}");
        assert!(!msg.contains("dispatch.single_shot (hosted)"), "nothing was sent: {msg}");
        assert_eq!(
            env.actions(),
            vec![darkmux_flow::FlowAction::BudgetWait, darkmux_flow::FlowAction::BudgetStop],
            "the ended wait is recorded as a stop"
        );
    }

    /// Two concurrent launches of ONE config: the same step `s1` in the same
    /// task `t1`, each launch its own run. Both wait on a full budgeted
    /// endpoint. Launch A's wait is stopped (its abort), and A is aborted on
    /// disk (what `darkmux mission abort` writes); launch B's window frees
    /// after 90 s. A's abort ends A's wait and only A's: B still waits, then
    /// resumes, and A's `budget.stop` names a session B never used, so no
    /// surface closes B's wait on it.
    #[test]
    #[serial_test::serial]
    fn an_abort_in_one_launch_never_ends_another_launchs_wait() {
        let crew = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", crew.path()) };
        for (mid, status) in [("launch-a", "aborted"), ("launch-b", "active")] {
            let mpath = crate::lifecycle::mission_path(mid);
            std::fs::create_dir_all(mpath.parent().unwrap()).unwrap();
            std::fs::write(&mpath, format!(r#"{{"id":"{mid}","status":"{status}"}}"#)).unwrap();
        }
        let reg = tempfile::TempDir::new().unwrap();
        let pf = reg.path().join("profiles.json");
        std::fs::write(
            &pf,
            r#"{"profiles":{"p":{"models":[{"id":"m","n_ctx":1}]}},
                "endpoints":{"azure":{"url":"http://127.0.0.1:1",
                    "limits":{"policy":"wait","window":{"period":"1d","tokens":1000}}}}}"#,
        )
        .unwrap();
        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({ "model": "gpt-5.1", "user": "hi", "endpoint": "azure", "config_path": pf.to_str().unwrap(), "timeout_seconds": 1 }),
        );
        // One launch of the config, as its launcher runs the step.
        let launch = |run: &str, env: std::rc::Rc<crate::budget::tests::FakeEnv>| {
            let ctx = StepRunCtx::solo(darkmux_types::session_id::RunId::mission(run).unwrap());
            let _ = crate::budget::with_test_env(env, || DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &ctx));
        };
        let env_a = std::rc::Rc::new(
            crate::budget::tests::FakeEnv::full_window().stopped_after(1, "mission `launch-a` is aborted"),
        );
        let env_b = std::rc::Rc::new(
            crate::budget::tests::FakeEnv::new(vec![(1_790_000_000 - 86_400 + 90, 1_000)]).reading_disk_for_stops(),
        );
        launch("launch-a", env_a.clone());
        launch("launch-b", env_b.clone());
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        use darkmux_flow::FlowAction::{BudgetResume, BudgetStop, BudgetWait};
        assert_eq!(env_a.actions(), vec![BudgetWait, BudgetStop], "A waits, then its abort ends the wait");
        assert_eq!(env_b.actions(), vec![BudgetWait, BudgetResume], "B reads only its own run: A's abort on disk does not end B's wait");
        let a_stop = env_a.record(BudgetStop).unwrap();
        let b_wait = env_b.record(BudgetWait).unwrap();
        assert_ne!(a_stop.session_id, b_wait.session_id, "A's stop must not close B's wait");
        assert_eq!(a_stop.mission_id.as_deref(), Some("launch-a"));
        assert_eq!(b_wait.mission_id.as_deref(), Some("launch-b"));
    }

    /// A registry naming endpoint `azure` at `url` (no limits), returning
    /// the tempdir guard and the registry path.
    fn azure_registry(url: &str) -> (tempfile::TempDir, String) {
        azure_registry_with(url, json!(null))
    }

    /// [`azure_registry`] with `limits` on the endpoint (`null`: none).
    fn azure_registry_with(url: &str, limits: serde_json::Value) -> (tempfile::TempDir, String) {
        let reg = tempfile::TempDir::new().unwrap();
        let pf = reg.path().join("profiles.json");
        std::fs::write(
            &pf,
            json!({
                "profiles": {"p": {"models": [{"id": "m", "n_ctx": 1}]}},
                "endpoints": {"azure": {"url": url, "limits": limits}},
            })
            .to_string(),
        )
        .unwrap();
        let path = pf.to_str().unwrap().to_string();
        (reg, path)
    }

    /// (#2902 step 5 review, 3rd pass MUST FIX 1) A hosted
    /// `dispatch.single_shot` against a NAMED endpoint stamps that id on its
    /// usage record (what the endpoint's window sums by), and settles the
    /// REPLY's total into the dispatch's token cap: a 10-token
    /// `tokens_per_dispatch` and a 12-token reply warn with `spent: 12`.
    /// Settling 0 (or the reservation) would leave the bucket silent.
    #[test]
    #[serial_test::serial]
    fn a_named_hosted_single_shot_stamps_its_endpoint_id_and_settles_the_reply() {
        let server = httpmock::MockServer::start();
        let mock = usage_mock(&server, true);
        let (_reg, pf) =
            azure_registry_with(&format!("{}/v1", server.base_url()), json!({"tokens_per_dispatch": 10}));
        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({ "model": "gpt-5.1", "user": "hi", "endpoint": "azure", "config_path": pf, "max_tokens": 5 }),
        );
        let env = std::rc::Rc::new(crate::budget::tests::FakeEnv::new(vec![]));
        let out = crate::budget::with_test_env(env.clone(), || {
            DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test())
        })
        .expect("mock answers");
        mock.assert_hits(1);
        let recs = as_values(&out.flow_records);
        let rec = crate::usage::assert_one_usage_record(&recs, crate::usage::CallKind::SingleShot, "single_shot (named)");
        assert_eq!(rec["payload"]["endpoint_id"], "azure", "{rec}");
        assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWarn]);
        let w = env.payload(darkmux_flow::FlowAction::BudgetWarn);
        assert_eq!(w["scope"], "dispatch");
        assert_eq!(w["spent"], 12, "the reply's total, not the 5-token grant: {w}");
        assert_eq!(w["limit"], 10);
    }

    /// A registry naming a MANAGED endpoint `lms` with `limits`, returning the
    /// tempdir guard and the registry path.
    fn lms_registry_with(limits: serde_json::Value) -> (tempfile::TempDir, String) {
        let reg = tempfile::TempDir::new().unwrap();
        let pf = reg.path().join("profiles.json");
        std::fs::write(
            &pf,
            json!({
                "profiles": {"p": {"models": [{"id": "m", "n_ctx": 1}]}},
                "endpoints": {"lms": {"managed": "lmstudio", "limits": limits}},
            })
            .to_string(),
        )
        .unwrap();
        let path = pf.to_str().unwrap().to_string();
        (reg, path)
    }

    /// (#3035) A LOCAL `dispatch.single_shot` whose `config.endpoint` names a
    /// MANAGED endpoint answers to its limits: a 10-token `tokens_per_dispatch`
    /// and a 12-token reply warn with `spent: 12`, and the usage record names
    /// the endpoint's id.
    #[test]
    #[serial_test::serial]
    fn a_managed_endpoint_step_settles_its_dispatch_cap_and_stamps_its_id() {
        let server = httpmock::MockServer::start();
        let mock = usage_mock(&server, true);
        let (_reg, pf) = lms_registry_with(json!({"tokens_per_dispatch": 10}));
        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({ "model": "m", "n_ctx": 1, "user": "hi", "endpoint": "lms", "config_path": pf, "max_tokens": 5 }),
        );
        let env = std::rc::Rc::new(crate::budget::tests::FakeEnv::new(vec![]));
        let url = server.base_url();
        let prev = std::env::var("DARKMUX_LMSTUDIO_URL").ok();
        unsafe { std::env::set_var("DARKMUX_LMSTUDIO_URL", &url) };
        let out = crate::budget::with_test_env(env.clone(), || {
            DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test())
        });
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LMSTUDIO_URL", v),
                None => std::env::remove_var("DARKMUX_LMSTUDIO_URL"),
            }
        }
        let out = out.expect("the mock answers");
        mock.assert_hits(1);
        let recs = as_values(&out.flow_records);
        let rec = crate::usage::assert_one_usage_record(&recs, crate::usage::CallKind::SingleShot, "single_shot (managed)");
        assert_eq!(rec["payload"]["endpoint_id"], "lms", "{rec}");
        assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWarn]);
        let w = env.payload(darkmux_flow::FlowAction::BudgetWarn);
        assert_eq!((w["spent"].as_u64(), w["limit"].as_u64()), (Some(12), Some(10)), "{w}");
    }

    /// (#3035, #1442 C4) A managed step call whose reply reports no usage is
    /// charged the granted cap (5) plus its prompt, so a 10-token cap warns.
    #[test]
    #[serial_test::serial]
    fn a_managed_endpoint_step_that_reports_no_usage_is_charged_the_granted_cap() {
        let server = httpmock::MockServer::start();
        let _mock = usage_mock(&server, false);
        let (_reg, pf) = lms_registry_with(json!({"tokens_per_dispatch": 10}));
        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({ "model": "m", "n_ctx": 1, "user": "hi", "endpoint": "lms", "config_path": pf, "max_tokens": 50 }),
        );
        let env = std::rc::Rc::new(crate::budget::tests::FakeEnv::new(vec![]));
        let prev = std::env::var("DARKMUX_LMSTUDIO_URL").ok();
        unsafe { std::env::set_var("DARKMUX_LMSTUDIO_URL", server.base_url()) };
        let out = crate::budget::with_test_env(env.clone(), || {
            DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test())
        });
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LMSTUDIO_URL", v),
                None => std::env::remove_var("DARKMUX_LMSTUDIO_URL"),
            }
        }
        out.expect("the mock answers");
        let w = env.payload(darkmux_flow::FlowAction::BudgetWarn);
        assert!(w["spent"].as_u64().unwrap() >= 50, "{w}");
    }

    /// (#3035) A local `dispatch.map` on a managed endpoint: each item is one
    /// dispatch with its own cap, so two 12-token replies against a 10-token
    /// cap warn twice (once per item), never once for the step.
    #[test]
    #[serial_test::serial]
    fn a_managed_endpoint_map_gives_each_item_its_own_dispatch_cap() {
        let server = httpmock::MockServer::start();
        let _mock = usage_mock(&server, true);
        let (_reg, pf) = lms_registry_with(json!({"tokens_per_dispatch": 10}));
        let s = map_step(json!({
            "model": "m", "n_ctx": 1, "user_template": "check {item}", "collection": ["a", "b"],
            "endpoint": "lms", "config_path": pf, "max_tokens": 5,
        }));
        let env = std::rc::Rc::new(crate::budget::tests::FakeEnv::new(vec![]));
        let prev = std::env::var("DARKMUX_LMSTUDIO_URL").ok();
        unsafe { std::env::set_var("DARKMUX_LMSTUDIO_URL", server.base_url()) };
        let out = crate::budget::with_test_env(env.clone(), || {
            DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test())
        });
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LMSTUDIO_URL", v),
                None => std::env::remove_var("DARKMUX_LMSTUDIO_URL"),
            }
        }
        let results: Vec<MapItemResult> = serde_json::from_str(&out.expect("the mock answers").output).unwrap();
        assert!(results.iter().all(|r| r.ok), "{results:?}");
        assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWarn; 2], "one warning per item");
    }

    /// (#2902 step 5 review, zero doctrine) A per-dispatch cap of 0 is NO
    /// cap: a 12-token reply against `0` warns nothing and reports no budget,
    /// where a real cap of 10 warns (the test above).
    #[test]
    #[serial_test::serial]
    fn a_zero_dispatch_cap_is_no_cap_and_never_warns() {
        let server = httpmock::MockServer::start();
        let _mock = usage_mock(&server, true);
        let (_reg, pf) =
            azure_registry_with(&format!("{}/v1", server.base_url()), json!({"tokens_per_dispatch": 0}));
        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({ "model": "gpt-5.1", "user": "hi", "endpoint": "azure", "config_path": pf, "max_tokens": 5 }),
        );
        let env = std::rc::Rc::new(crate::budget::tests::FakeEnv::new(vec![]));
        let out = crate::budget::with_test_env(env.clone(), || {
            DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test())
        })
        .expect("mock answers");
        assert!(env.actions().is_empty(), "no cap, no warning: {:?}", env.actions());
        let recs = as_values(&out.flow_records);
        let step_rec = recs
            .iter()
            .find(|r| r["action"] == "step.result" && r["payload"].get("max_tokens_sent").is_some())
            .unwrap_or_else(|| panic!("the step's telemetry record: {recs:#?}"));
        assert!(step_rec["payload"].get("tokens_per_dispatch").is_none(), "no cap is reported: {step_rec}");
    }

    /// (#2902 step 5 review, 3rd pass MUST FIX 1) A hosted `dispatch.map`
    /// item against a NAMED endpoint stamps that id on its per-call usage
    /// record.
    #[test]
    fn a_named_hosted_map_item_stamps_its_endpoint_id() {
        let server = httpmock::MockServer::start();
        let mock = usage_mock(&server, true);
        let (_reg, pf) = azure_registry(&format!("{}/v1", server.base_url()));
        let s = map_step(json!({
            "model": "gpt-5.1", "user_template": "check {item}", "collection": ["a"],
            "endpoint": "azure", "config_path": pf,
        }));
        let env = std::rc::Rc::new(crate::budget::tests::FakeEnv::new(vec![]));
        let out = crate::budget::with_test_env(env.clone(), || {
            DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test())
        })
        .expect("mock answers");
        mock.assert_hits(1);
        let recs = as_values(&out.flow_records);
        let rec = crate::usage::assert_one_usage_record(&recs, crate::usage::CallKind::MapItem, "map item (named)");
        assert_eq!(rec["payload"]["endpoint_id"], "azure", "{rec}");
    }

    #[test]
    fn conservative_hosted_spend_charges_the_granted_cap_when_usage_is_omitted() {
        // (#1442 gate C4) A reply that reports usage spends what it reports;
        // a reply that OMITS usage spends the clamped max_tokens it was
        // granted — an omitting endpoint must not mint an infinite allowance.
        let empty = serde_json::json!({});
        let prompt = darkmux_trajectory::estimate_tokens(&empty.to_string()) as u64;
        assert_eq!(crate::budget::conservative_hosted_spend(Some(1234), 4096, &empty), 1234);
        assert_eq!(crate::budget::conservative_hosted_spend(None, 4096, &empty), 4096 + prompt);
        assert_eq!(crate::budget::conservative_hosted_spend(None, 0, &empty), prompt);
    }

    /// (#1530 dogfood) A map item's `telemetry.tokens` record carries the
    /// prompt/completion SPLIT, not just the total.
    ///
    /// This is the regression the 2.3.0 dogfood caught in production. The
    /// fleet dashboard's `tokensOffMeter()` sums `total_tokens` into the
    /// headline but CLASSIFIES via `prompt_tokens`/`completion_tokens`, so a
    /// total-only record is counted and then bucketed nowhere: 62,047 of
    /// 152,271 local tokens (41%) were headline-visible and chip-invisible
    /// after the review probe/verify stages moved onto `dispatch.map`
    /// (#1442). `SingleShotReply` had carried the split since #1361; it was
    /// `MapItemResult` that dropped it on the floor.
    /// (#1524) Every `dispatch.map` emit carries the CANONICAL hyphen-form
    /// session id, not the pre-#1436 colon form.
    ///
    /// `mission_graph.rs` deliberately REFUSES colon-era ids (its own test
    /// `fold_finals_colon_era_session_ids_do_not_fold` pins that refusal), so
    /// a colon-form emit is silently invisible to the graph lens's token
    /// meters — the #1445 blank-meter class, recurring on the dispatch.map
    /// path. The sibling `dispatch.single_shot` emitter already used the
    /// helper; these four sites had drifted to an inline `format!`.
    ///
    /// The id comes from `SessionId::task`, so a future grammar change moves
    /// both producer and consumer together instead of silently re-breaking
    /// the meters.
    #[test]
    fn map_emits_canonical_task_session_ids_not_colon_form() {
        let expected = darkmux_types::session_id::SessionId::task(crate::test_run(), "t1").wire();
        assert!(
            !expected.contains(':'),
            "canonical form must not be colon-delimited, got {expected}"
        );
        assert_eq!(expected, "m-test.task.t1", "the task session in its run");

        // The aggregate record is the one a reader can build without a live
        // dispatch; the per-item emits use the identical expression.
        let s = map_step(json!({}));
        let results = vec![MapItemResult {
            index: 0,
            ok: true,
            content: "x".to_string(),
            error: None,
            total_tokens: Some(10),
            prompt_tokens: None,
            completion_tokens: None,
            reasoning_tokens: None,
            cached_tokens: None,
            served_model: None,
            wall_ms: 0,
            retried: 0,
        }];
        let rec = DispatchMapStepKind::aggregate_record(&task_session(), &s, "m", false, &results);
        assert_eq!(
            rec.session_id.as_deref(),
            Some(expected.as_str()),
            "aggregate record must carry the canonical id or the graph meters stay blank"
        );
    }

    /// (#1444 review) The hosted `dispatch.single_shot` step's "step result"
    /// record. This payload used to be an inline `json!` inside an arm that
    /// makes a real HTTP call with no override seam, so nothing executed it:
    /// replacing `reply.reasoning_tokens` with `Null` there left all 1503
    /// crew tests green. Extracted to `hosted_single_shot_step_payload` and
    /// pinned here.
    #[test]
    fn single_shot_step_telemetry_carries_reasoning_and_cached_tokens() {
        let reply = crate::single_shot::SingleShotReply {
            content: "ok".to_string(),
            model: Some("hosted".to_string()),
            counts: darkmux_trajectory::UsageCounts { total: Some(1261), prompt: Some(75), completion: Some(1186), reasoning: Some(1024), cached: Some(64), cache_write: None },
        };
        let payload = serde_json::to_value(hosted_single_shot_step_payload("s1", Some(500_000), 4096, 4096, &reply)).unwrap();
        assert_eq!(payload["reasoning_tokens"], 1024);
        assert_eq!(payload["cached_tokens"], 64);
        // Neighbors, so a copy-paste slip between fields cannot pass.
        assert_eq!(payload["prompt_tokens"], 75);
        assert_eq!(payload["completion_tokens"], 1186);
        assert_eq!(payload["total_tokens"], 1261);
        assert_eq!(payload["step_id"], "s1");
        assert_eq!(payload["kind"], "dispatch.single_shot");
        assert!(payload.get("runtime").is_none(), "no topology key: {payload}");
        assert_eq!(payload["max_tokens_sent"], 4096);
    }

    /// (#1444 review, 4.0 typed payloads) An endpoint that reported no details
    /// object leaves both keys ABSENT from the `step.result` payload, never a
    /// fabricated `0`: the payload is one type shared by every step producer,
    /// and an unreported count is an omitted key on it.
    #[test]
    fn single_shot_step_telemetry_omits_unreported_details() {
        let reply = crate::single_shot::SingleShotReply {
            content: "ok".to_string(),
            model: None,
            counts: darkmux_trajectory::UsageCounts { total: Some(42), prompt: Some(30), completion: Some(12), ..Default::default() },
        };
        let payload = serde_json::to_value(hosted_single_shot_step_payload("s1", Some(500_000), 4096, 4096, &reply)).unwrap();
        let obj = payload.as_object().expect("object");
        assert!(!obj.contains_key("reasoning_tokens"), "absent, never a fabricated 0");
        assert!(!obj.contains_key("cached_tokens"));
    }

    #[test]
    fn map_item_token_telemetry_carries_the_prompt_completion_split() {
        let res = MapItemResult {
            index: 0,
            ok: true,
            content: "x".to_string(),
            error: None,
            total_tokens: Some(4547),
            prompt_tokens: Some(2490),
            completion_tokens: Some(2057),
            reasoning_tokens: None,
            cached_tokens: None,
            served_model: None,
            wall_ms: 0,
            retried: 0,
        };
        let payload = serde_json::to_value(map_item_token_payload(&res, "m", "ep").expect("a reply with usage emits a record")).unwrap();
        assert_eq!(payload["total_tokens"], 4547);
        assert_eq!(payload["prompt_tokens"], 2490, "GENERATED/fresh/re-read read the split");
        assert_eq!(payload["completion_tokens"], 2057);
    }

    /// (#2690) `session_id` on a map item's `telemetry.tokens` record is the
    /// step's task session, which sibling seats inside one task SHARE, so the
    /// record carries the item's `index` to tell them apart. It carries no
    /// hosted/local flag: a token record names the endpoint and model it
    /// called, and a consumer classifies nothing from it.
    #[test]
    fn map_item_token_telemetry_carries_its_index_and_no_tier() {
        let local = MapItemResult {
            index: 3,
            ok: true,
            content: "x".to_string(),
            error: None,
            total_tokens: Some(100),
            prompt_tokens: Some(90),
            completion_tokens: Some(10),
            reasoning_tokens: None,
            cached_tokens: None,
            served_model: None,
            wall_ms: 0,
            retried: 0,
        };
        let payload = serde_json::to_value(map_item_token_payload(&local, "m", "ep").expect("emits")).unwrap();
        assert_eq!(payload["index"], 3, "which item of the fan-out this was");
        assert!(payload.get("remote").is_none(), "a token record names its endpoint and model, never a hosted/local tier: {payload}");
    }

    /// Each map item's `step result` record names that item's own execution,
    /// the one its bookends and usage records carry, not just the task session
    /// the items share.
    #[test]
    fn a_map_items_step_result_names_its_execution() {
        let step = map_step(json!({}));
        let res = MapItemResult {
            index: 0,
            ok: true,
            content: String::new(),
            error: None,
            total_tokens: None,
            prompt_tokens: None,
            completion_tokens: None,
            reasoning_tokens: None,
            cached_tokens: None,
            served_model: None,
            wall_ms: 0,
            retried: 0,
        };
        let execution = ExecutionId::mint();
        let rec = DispatchMapStepKind::item_record(&task_session(), &execution, &step, "m", false, &res);
        assert_eq!(rec.execution_id.as_ref(), Some(&execution));
    }

    /// (#2690) The item position a map step stamps on its `telemetry.tokens`
    /// record and the one it stamps on that same item's `step result` record
    /// are the same fact; pinned together so a future edit cannot move one
    /// without the other.
    #[test]
    fn map_item_index_agrees_between_the_token_record_and_the_step_result() {
        let step = map_step(json!({}));
        let res = MapItemResult {
            index: 1,
            ok: true,
            content: String::new(),
            error: None,
            total_tokens: Some(42),
            prompt_tokens: None,
            completion_tokens: None,
            reasoning_tokens: None,
            cached_tokens: None,
            served_model: None,
            wall_ms: 0,
            retried: 0,
        };
        let tok = serde_json::to_value(map_item_token_payload(&res, "m", "ep").expect("emits")).unwrap();
        let item = DispatchMapStepKind::item_record(&task_session(), &ExecutionId::mint(), &step, "m", false, &res);
        assert_eq!(tok["index"], item.payload_json()["index"], "one item position");
    }

    /// (#1444 review) `dispatch.map` is the highest-volume hosted-remote
    /// path in the product — the review funnel's probe and verify stages run
    /// on it — and #1444's first pass gave it NO reasoning coverage at all:
    /// `SingleShotReply` carried both new fields, `MapItemResult` dropped
    /// them, and `accumulate_split` folded only prompt and completion. The
    /// 1.44.0 schema entry claimed `telemetry.tokens` coverage while this
    /// producer had none — exactly the loss #1530 closed for the split, one
    /// field-pair later.
    #[test]
    fn map_item_token_telemetry_carries_reasoning_and_cached_tokens() {
        let res = MapItemResult {
            index: 0,
            ok: true,
            content: "x".to_string(),
            error: None,
            total_tokens: Some(1261),
            prompt_tokens: Some(75),
            completion_tokens: Some(1186),
            reasoning_tokens: Some(1024),
            cached_tokens: Some(64),
            served_model: None,
            wall_ms: 0,
            retried: 0,
        };
        let payload = serde_json::to_value(map_item_token_payload(&res, "m", "ep").expect("a reply with usage emits a record")).unwrap();
        assert_eq!(payload["reasoning_tokens"], 1024);
        assert_eq!(payload["cached_tokens"], 64);
        // The neighbors must still land where they belong.
        assert_eq!(payload["total_tokens"], 1261);
        assert_eq!(payload["prompt_tokens"], 75);
        assert_eq!(payload["completion_tokens"], 1186);
    }

    /// (#1444 review) The omit-never-fabricate rule extends to the two new
    /// fields, and each is independent: an item whose provider reported
    /// reasoning but no `prompt_tokens_details` emits `reasoning_tokens`
    /// and leaves `cached_tokens` off entirely — never a zero.
    #[test]
    fn map_item_token_telemetry_omits_unreported_details_independently() {
        let res = MapItemResult {
            index: 0,
            ok: true,
            content: "x".to_string(),
            error: None,
            total_tokens: Some(550),
            prompt_tokens: Some(200),
            completion_tokens: Some(350),
            reasoning_tokens: Some(300),
            cached_tokens: None,
            served_model: None,
            wall_ms: 0,
            retried: 0,
        };
        let payload = serde_json::to_value(map_item_token_payload(&res, "m", "ep").expect("emits")).unwrap();
        assert_eq!(payload["reasoning_tokens"], 300);
        assert!(
            payload.get("cached_tokens").is_none(),
            "an unreported field is omitted, never zeroed — and never dragged \
             along by its sibling being present"
        );

        let neither = MapItemResult { reasoning_tokens: None, cached_tokens: None, ..res };
        let payload = serde_json::to_value(map_item_token_payload(&neither, "m", "ep").expect("emits")).unwrap();
        assert!(payload.get("reasoning_tokens").is_none());
        assert!(payload.get("cached_tokens").is_none());
    }

    /// (#1444 review) An item's token fields are the sum of its calls'
    /// counts, each field on its own: a provider can name
    /// `completion_tokens_details` without `prompt_tokens_details`, so a
    /// shared "was it reported" flag would fabricate a `0` for whichever one
    /// went unnamed.
    #[test]
    fn an_items_tokens_sum_each_reported_field_independently() {
        let call = |counts| MapCall { counts, reported_model: None };
        let none = MapItemResult::tokens_of(&[call(darkmux_trajectory::UsageCounts::default())]);
        assert_eq!((none.total_tokens, none.reasoning_tokens, none.cached_tokens), (None, None, None));

        // Attempt 1 reports both details; attempt 2 reasoning only.
        let both = MapItemResult::tokens_of(&[
            call(darkmux_trajectory::UsageCounts { reasoning: Some(500), cached: Some(20), ..Default::default() }),
            call(darkmux_trajectory::UsageCounts { reasoning: Some(300), ..Default::default() }),
        ]);
        assert_eq!(both.reasoning_tokens, Some(800), "500 + 300 across attempts");
        assert_eq!(both.cached_tokens, Some(20), "attempt 2's silence must not reset what attempt 1 reported");

        // The mirror case: cached reported, reasoning never.
        let mirror = MapItemResult::tokens_of(&[call(darkmux_trajectory::UsageCounts { cached: Some(64), ..Default::default() })]);
        assert_eq!(mirror.reasoning_tokens, None, "a shared flag would have fabricated Some(0) here");
        assert_eq!(mirror.cached_tokens, Some(64));
    }

    /// The item's total is the sum of what its usage records carry, one per
    /// call: a call that reported only a split counts its prompt +
    /// completion, exactly as its record's `total_tokens` does. (The item
    /// used to sum provider totals alone and dropped such a call.)
    #[test]
    fn an_items_total_is_the_sum_of_its_calls_usage_record_totals() {
        let calls = [
            MapCall { counts: darkmux_trajectory::UsageCounts { prompt: Some(30), completion: Some(12), ..Default::default() }, reported_model: None },
            MapCall { counts: darkmux_trajectory::UsageCounts { total: Some(100), prompt: Some(60), completion: Some(20), ..Default::default() }, reported_model: None },
        ];
        let item = MapItemResult::tokens_of(&calls);
        let records: u64 = calls
            .iter()
            .map(|c| map_call_token_payload(c, 0, "m", "ep", None))
            .map(|p| p.total_tokens.unwrap_or(0))
            .sum();
        assert_eq!(item.total_tokens, Some(142), "42 from the split + the provider's own 100");
        assert_eq!(item.total_tokens, Some(records), "the item and its records are one number");
        assert_eq!((item.prompt_tokens, item.completion_tokens), (Some(90), Some(32)));
    }

    /// The no-fabrication rule survives the fix: a provider reporting only a
    /// total leaves the split OFF the payload entirely rather than claiming
    /// a zero, which the dashboard would read as "generated nothing".
    #[test]
    fn map_item_token_telemetry_omits_an_unreported_split() {
        let res = MapItemResult {
            index: 0,
            ok: true,
            content: "x".to_string(),
            error: None,
            total_tokens: Some(1521),
            prompt_tokens: None,
            completion_tokens: None,
            reasoning_tokens: None,
            cached_tokens: None,
            served_model: None,
            wall_ms: 0,
            retried: 0,
        };
        let payload = serde_json::to_value(map_item_token_payload(&res, "m", "ep").expect("a total alone still emits")).unwrap();
        assert_eq!(payload["total_tokens"], 1521);
        assert!(payload.get("prompt_tokens").is_none(), "never fabricate a split");
        assert!(payload.get("completion_tokens").is_none());
    }

    /// (#1530 dogfood, gate C1) A provider that reports a SPLIT but no total
    /// still emits — the total is summed from the parts. `extract_reply`
    /// reads all three `usage` fields independently, so this combination is
    /// representable, and the old `total_tokens?` gate would have dropped
    /// the record entirely, losing a split the item genuinely had.
    #[test]
    fn map_item_token_telemetry_derives_a_missing_total_from_the_split() {
        let res = MapItemResult {
            index: 0,
            ok: true,
            content: "x".to_string(),
            error: None,
            total_tokens: None,
            prompt_tokens: Some(30),
            completion_tokens: Some(12),
            reasoning_tokens: None,
            cached_tokens: None,
            served_model: None,
            wall_ms: 0,
            retried: 0,
        };
        let payload = serde_json::to_value(map_item_token_payload(&res, "m", "ep").expect("a split alone still emits")).unwrap();
        assert_eq!(payload["total_tokens"], 42, "arithmetic on reported parts, not fabrication");
        assert_eq!(payload["prompt_tokens"], 30);
        assert_eq!(payload["completion_tokens"], 12);
    }


    /// The split sums across attempts and stays `None` when never reported.
    /// A half-reported split counts its half, and the other half stays
    /// unreported, as the call's usage record omits it.
    #[test]
    fn split_accumulates_across_attempts_and_stays_absent_when_never_reported() {
        let call = |prompt, completion| MapCall {
            counts: darkmux_trajectory::UsageCounts { prompt, completion, ..Default::default() },
            reported_model: None,
        };
        let never = MapItemResult::tokens_of(&[call(None, None)]);
        assert_eq!((never.prompt_tokens, never.completion_tokens), (None, None));
        let both = MapItemResult::tokens_of(&[call(None, None), call(Some(10), Some(4)), call(Some(7), Some(3))]);
        assert_eq!((both.prompt_tokens, both.completion_tokens), (Some(17), Some(7)));
        let half = MapItemResult::tokens_of(&[call(Some(5), None)]);
        assert_eq!((half.prompt_tokens, half.completion_tokens), (Some(5), None));
    }

    #[test]
    fn dispatch_map_aggregate_record_sums_tokens_and_counts_outcomes() {
        // (#1442 gate C1) The one step-level aggregate: items_in, ok_count,
        // failed_count, unmanaged, and SUMMED total_tokens — the record the
        // mission graph's max-fold token meter reads as the step's true
        // spend (any per-item value is <= the sum).
        let results = vec![
            MapItemResult { index: 0, ok: true, content: "a".to_string(), error: None, total_tokens: Some(100), prompt_tokens: None, completion_tokens: None, reasoning_tokens: None, cached_tokens: None, served_model: None, wall_ms: 0, retried: 0 },
            MapItemResult { index: 1, ok: false, content: String::new(), error: Some("boom".to_string()), total_tokens: None, prompt_tokens: None, completion_tokens: None, reasoning_tokens: None, cached_tokens: None, served_model: None, wall_ms: 0, retried: 0 },
            MapItemResult { index: 2, ok: true, content: "c".to_string(), error: None, total_tokens: Some(250), prompt_tokens: None, completion_tokens: None, reasoning_tokens: None, cached_tokens: None, served_model: None, wall_ms: 0, retried: 0 },
        ];
        let s = map_step(json!({}));
        let rec = DispatchMapStepKind::aggregate_record(&task_session(), &s, "m", true, &results);
        let p = rec.payload_json();
        assert_eq!(p["kind"], "dispatch.map");
        assert_eq!(p["items_in"], 3);
        assert_eq!(p["ok_count"], 2);
        assert_eq!(p["failed_count"], 1);
        assert_eq!(p["unmanaged"], true);
        assert_eq!(p["total_tokens"], 350, "summed across items, absent usage counted as 0 here");
        assert!(
            matches!(rec.level, darkmux_flow::Level::Warn),
            "any failed item raises the level"
        );

        let clean = vec![MapItemResult { index: 0, ok: true, content: "a".to_string(), error: None, total_tokens: Some(5), prompt_tokens: None, completion_tokens: None, reasoning_tokens: None, cached_tokens: None, served_model: None, wall_ms: 0, retried: 0 }];
        let rec = DispatchMapStepKind::aggregate_record(&task_session(), &s, "m", false, &clean);
        assert!(matches!(rec.level, darkmux_flow::Level::Info));
        assert_eq!(rec.payload_json()["unmanaged"], false);
    }

    fn map_item(index: usize, ok: bool) -> MapItemResult {
        MapItemResult {
            index,
            ok,
            content: String::new(),
            error: (!ok).then(|| format!("item {index} boom")),
            total_tokens: None,
            prompt_tokens: None,
            completion_tokens: None,
            reasoning_tokens: None,
            cached_tokens: None,
            served_model: None,
            wall_ms: 0,
            retried: 0,
        }
    }

    fn run_of(results: &[MapItemResult]) -> MapRun {
        MapRun {
            outcome: StepOutcome { output: serde_json::to_string(results).unwrap(), flow_records: Vec::new(), degraded: None },
            verdict: MapVerdict::of(results),
        }
    }

    /// The step's verdict follows its items: every item failing is the step
    /// failing (nothing usable came out), some failing is a degraded step whose
    /// output is still delivered, none failing is a clean step.
    #[test]
    fn map_run_verdict_maps_item_outcomes_to_the_step_outcome() {
        let all_failed = run_of(&[map_item(0, false), map_item(1, false)]).into_outcome("m1");
        let msg = format!("{:#}", all_failed.expect_err("every item failed, so the step errors"));
        assert!(msg.contains("all 2 item(s) failed") && msg.contains("item 0 boom"), "{msg}");

        let partial = run_of(&[map_item(0, true), map_item(1, false), map_item(2, true)]).into_outcome("m1").unwrap();
        assert_eq!(partial.degraded.as_deref(), Some("dispatch.map step `m1`: 1 of 3 item(s) failed"));
        let kept: Vec<MapItemResult> = serde_json::from_str(&partial.output).unwrap();
        assert_eq!(kept.len(), 3, "a degraded step still delivers every item's result");

        let clean = run_of(&[map_item(0, true), map_item(1, true)]).into_outcome("m1").unwrap();
        assert_eq!(clean.degraded, None);
    }

    /// Through the real step kind: a map whose every item cannot reach its
    /// endpoint is an errored step, not a completed one that reads Clean.
    #[test]
    #[serial_test::serial]
    fn dispatch_map_where_every_item_failed_errors_the_step() {
        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev = std::env::var(url_key).ok();
        unsafe { std::env::set_var(url_key, "http://127.0.0.1:1") };
        let s = map_step(json!({
            "model": "m",
            "user_template": "check {item}",
            "collection": ["a", "b"],
            "timeout_seconds": 1,
        }));
        let result = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test());
        unsafe {
            match prev {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }
        let msg = format!("{:#}", result.expect_err("all items failed"));
        assert!(msg.contains("all 2 item(s) failed"), "{msg}");
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_local_per_item_error_isolation_continues_past_a_failure() {
        // Point the local dialect at an unroutable endpoint (port 1 refuses
        // immediately, 1s timeout) so EVERY item's dispatch errors — the
        // policy under test is that each failure is CAPTURED into that item's
        // result and the loop CONTINUES to the next, rather than the first
        // error aborting the whole step. Three items in -> three ok:false
        // results out (the step-level verdict, an error when every item
        // failed, is `map_run_verdict_maps_item_outcomes_to_the_step_outcome`).
        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev = std::env::var(url_key).ok();
        unsafe {
            std::env::set_var(url_key, "http://127.0.0.1:1");
        }
        let s = map_step(json!({
            "model": "m",
            "user_template": "check {item}",
            "collection": ["a", "b", "c"],
            "timeout_seconds": 1,
        }));
        let out = DispatchMapStepKind.run_map(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap().outcome;
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 3, "every item produced a result despite each failing");
        assert!(results.iter().all(|r| !r.ok), "each item's dispatch failed and was isolated");
        assert!(results.iter().all(|r| r.error.is_some()), "each failure named");
        assert_eq!(results[0].index, 0);
        assert_eq!(results[2].index, 2);
        // A per-item flow record for every item, PLUS the one step-level
        // aggregate after the loop (#1442 gate C1) — 3 + 1.
        assert_eq!(out.flow_records.len(), 4);
        let agg = out.flow_records.last().unwrap().payload_json();
        assert_eq!(agg["items_in"], 3);
        assert_eq!(agg["ok_count"], 0);
        assert_eq!(agg["failed_count"], 3);
        assert_eq!(agg["total_tokens"], 0);
        unsafe {
            match prev {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_single_item_degenerate_case_still_produces_one_result() {
        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev = std::env::var(url_key).ok();
        unsafe {
            std::env::set_var(url_key, "http://127.0.0.1:1");
        }
        let s = map_step(json!({
            "model": "m",
            "user_template": "check {item}",
            "collection": ["only"],
            "timeout_seconds": 1,
        }));
        let out = DispatchMapStepKind.run_map(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap().outcome;
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].index, 0);
        unsafe {
            match prev {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }
    }

    /// (#2902 step 5) A dispatch cap of 0 under the default `warn` skips
    /// nothing: every hosted item is dispatched (counted through the
    /// override), each is ok. Pre-4.0 every item was skipped.
    #[test]
    #[serial_test::serial]
    fn dispatch_map_a_zero_step_cap_under_warn_dispatches_every_item() {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let seen = calls.clone();
        clear_hosted_override();
        install_hosted_override(move |_req| {
            seen.set(seen.get() + 1);
            Ok(hosted_reply(Some(10)))
        });
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a", "b", "c"],
            "endpoint": { "url": "https://example.com", "limits": { "tokens_per_dispatch": 0 } },
        }));
        let out = DispatchMapStepKind
            .run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test())
            .unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.ok), "{results:?}");
        assert_eq!(calls.get(), 3, "every item dispatched");
    }

    // ── (#1442 gate) dispatch.map hosted-seam tests ─────────────────────
    // The hosted override (MAP_HOSTED_OVERRIDE, the unit-struct builtin's
    // equivalent of review.rs's chat_override) makes item 1 a GENUINE
    // successful hosted dispatch that spends, so mid-collection exhaustion
    // (items 2+ skip) is exercisable without a network — the coverage the
    // budget-0 test (whole collection skipped) cannot reach.

    fn install_hosted_override(
        f: impl Fn(&crate::single_shot::HostedSingleShotRequest) -> Result<crate::single_shot::SingleShotReply>
            + 'static,
    ) {
        MAP_HOSTED_OVERRIDE.with(|o| *o.borrow_mut() = Some(Box::new(f)));
    }
    fn clear_hosted_override() {
        MAP_HOSTED_OVERRIDE.with(|o| *o.borrow_mut() = None);
    }
    fn hosted_reply(total: Option<u64>) -> crate::single_shot::SingleShotReply {
        crate::single_shot::SingleShotReply {
            content: "flag".to_string(),
            model: Some("hosted".to_string()),
            counts: darkmux_trajectory::UsageCounts { total, ..Default::default() },
        }
    }

    /// (#3035) Each item is one dispatch with its own 100-token cap. Every
    /// item spends all of it, so each warns once (three warnings, not one
    /// shared allowance), and under the default `warn` all three are still
    /// dispatched and ok, each reporting its real usage.
    #[test]
    #[serial_test::serial]
    fn dispatch_map_each_item_has_its_own_dispatch_cap_and_keeps_going_under_warn() {
        let _state = darkmux_types::test_isolation::IsolatedState::new(); // pins HOME/DARKMUX_HOME: the step budget's `budget.warn` goes to the real flow sink otherwise
        clear_hosted_override();
        install_hosted_override(|_req| Ok(hosted_reply(Some(100))));
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a", "b", "c"],
            "endpoint": { "url": "https://example.com", "limits": { "tokens_per_dispatch": 100 } },
        }));
        let env = std::rc::Rc::new(crate::budget::tests::FakeEnv::new(vec![]));
        let out = crate::budget::with_test_env(env.clone(), || {
            DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap()
        });
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 3);
        for r in &results {
            assert!(r.ok, "item {} dispatched: {r:?}", r.index);
            assert_eq!(r.total_tokens, Some(100));
        }
        assert_eq!(
            env.actions(),
            vec![darkmux_flow::FlowAction::BudgetWarn; 3],
            "one warning per item: each item is its own dispatch with its own cap"
        );
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_hosted_reply_without_usage_stays_honest_none_at_run_level() {
        // An endpoint that omits usage entirely: the item is `ok` (it
        // dispatched) but its `total_tokens` is honest `None` — the run-level
        // result array never fabricates a number the endpoint didn't send.
        // (The bucket still charges the conservative clamped grant so an
        // omitting endpoint can't run the whole collection off the meter.)
        clear_hosted_override();
        install_hosted_override(|_req| Ok(hosted_reply(None)));
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a", "b"],
            "endpoint": { "url": "https://example.com" },
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.ok), "both dispatched");
        assert!(
            results.iter().all(|r| r.total_tokens.is_none()),
            "no fabricated token count when the endpoint omitted usage"
        );
    }

    // ── (#1442) dispatch.map retry_on_empty ─────────────────────────────

    /// Script a sequence of hosted replies (content, usage) — the closure
    /// walks the script one entry per call, clamping to the last entry once
    /// exhausted (so a longer-than-scripted run keeps returning the tail).
    fn install_scripted_hosted(script: Vec<(&'static str, Option<u64>)>) {
        let idx = std::cell::Cell::new(0usize);
        let script = std::rc::Rc::new(script);
        install_hosted_override(move |_req| {
            let i = idx.get().min(script.len().saturating_sub(1));
            idx.set(idx.get() + 1);
            let (content, total) = script[i];
            Ok(crate::single_shot::SingleShotReply {
                content: content.to_string(),
                model: Some("hosted".to_string()),
                counts: darkmux_trajectory::UsageCounts { total, ..Default::default() },
            })
        });
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_retry_on_empty_retries_then_succeeds() {
        // First attempt returns empty (but bills 50), the retry returns real
        // content (bills 70). retry_on_empty=1 → the item ends ok with the
        // non-empty content and tokens SUMMED across both attempts.
        clear_hosted_override();
        install_scripted_hosted(vec![("", Some(50)), ("flag", Some(70))]);
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
            "retry_on_empty": 1,
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].ok, "the retry produced usable content");
        assert_eq!(results[0].content, "flag");
        assert_eq!(results[0].total_tokens, Some(120), "tokens billed across BOTH attempts");
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_retry_on_empty_gives_up_honestly() {
        // Both attempts empty (bill 50 + 60). retry_on_empty=1 exhausts, and
        // the item ends ok:true with EMPTY content (dispatched, no usable
        // result) and the full spend billed — never a flag from nothing.
        clear_hosted_override();
        install_scripted_hosted(vec![("", Some(50)), ("   ", Some(60))]);
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
            "retry_on_empty": 1,
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].ok, "it dispatched — the empty content is a real, honest zero");
        assert!(results[0].content.is_empty(), "no usable content after the retries");
        assert_eq!(results[0].total_tokens, Some(110), "every attempt's spend billed");
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_retry_on_empty_default_off_accepts_the_first_empty_reply() {
        // With no retry_on_empty configured (default 0), an empty reply is
        // accepted as-is on the FIRST attempt — one call, tokens from it only.
        clear_hosted_override();
        // Second entry would be non-empty — if a retry (wrongly) fired we'd
        // see "would-be-retry" content and 90 total tokens instead.
        install_scripted_hosted(vec![("", Some(40)), ("would-be-retry", Some(50))]);
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].ok);
        assert!(results[0].content.is_empty(), "default off does not retry the empty reply");
        assert_eq!(results[0].total_tokens, Some(40), "exactly one call was made");
    }

    // ── (#1605) dispatch.map retry_on_error ──────────────────────────────
    //
    // darkmux issue #1605 cause 2: a probe stage where EVERY draw errored
    // together read like a transient endpoint outage in the sampled data,
    // so the review probe stage opts a `dispatch.map` step into a bounded
    // ONE-retry-with-backoff via this generic (default-off) config knob.
    // These tests pin the mechanism itself; `review.rs`'s own tests pin
    // that probe opts in and verify does not.

    /// Script a sequence of hosted call OUTCOMES (`Ok`/`Err`) — walks one
    /// entry per call, clamping to the last entry once exhausted, and counts
    /// how many calls actually fired (so a test can assert the retry
    /// happened exactly the expected number of times, not just that the
    /// final result looks right).
    fn install_scripted_hosted_outcomes(
        script: Vec<Result<(&'static str, Option<u64>)>>,
    ) -> std::rc::Rc<std::cell::Cell<usize>> {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let calls_inner = calls.clone();
        let idx = std::cell::Cell::new(0usize);
        let script = std::rc::Rc::new(script);
        install_hosted_override(move |_req| {
            calls_inner.set(calls_inner.get() + 1);
            let i = idx.get().min(script.len().saturating_sub(1));
            idx.set(idx.get() + 1);
            match &script[i] {
                Ok((content, total)) => Ok(crate::single_shot::SingleShotReply {
                    content: content.to_string(),
                    model: Some("hosted".to_string()),
                    counts: darkmux_trajectory::UsageCounts { total: *total, ..Default::default() },
                }),
                Err(e) => Err(anyhow!("{e}")),
            }
        });
        calls
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_retry_on_error_retries_then_succeeds() {
        // First attempt errors (a transient blip); retry_on_error=1 fires
        // ONE retry, which succeeds. The item ends ok:true and exactly TWO
        // calls fired — not zero, not more than the bounded budget.
        clear_hosted_override();
        let calls = install_scripted_hosted_outcomes(vec![
            Err(anyhow!("transient: connection reset")),
            Ok(("flag", Some(70))),
        ]);
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
            "retry_on_error": 1,
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].ok, "the retry recovered the item");
        assert_eq!(results[0].content, "flag");
        assert_eq!(results[0].retried, 1, "exactly one error-retry was consumed");
        assert_eq!(calls.get(), 2, "exactly two calls fired: the failed attempt + the one retry");
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_retry_on_error_bounded_to_one_never_retries_twice() {
        // Every attempt errors. retry_on_error=1 permits exactly ONE retry —
        // two calls total, then the item isolates as ok:false carrying the
        // LAST attempt's error. A THIRD call would mean the bound leaked.
        clear_hosted_override();
        let calls = install_scripted_hosted_outcomes(vec![
            Err(anyhow!("first failure")),
            Err(anyhow!("second failure")),
            Ok(("would never be reached", Some(999))),
        ]);
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
            "retry_on_error": 1,
        }));
        let out = DispatchMapStepKind.run_map(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap().outcome;
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].ok, "both attempts failed — isolated, never fabricated a success");
        assert_eq!(results[0].retried, 1, "the one permitted retry was consumed");
        assert!(
            results[0].error.as_deref().unwrap().contains("second failure"),
            "the LAST attempt's error is what's recorded: {:?}",
            results[0].error
        );
        assert_eq!(calls.get(), 2, "bounded to exactly one retry — never a third call");
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_retry_on_error_default_off_isolates_immediately() {
        // With no `retry_on_error` configured (default 0/off — the ORIGINAL
        // policy, preserved for every caller that doesn't opt in), a single
        // dispatch error isolates on the FIRST attempt — exactly one call,
        // matching pre-#1605 behavior byte-for-byte.
        clear_hosted_override();
        let calls = install_scripted_hosted_outcomes(vec![
            Err(anyhow!("boom")),
            Ok(("would never be reached", Some(999))),
        ]);
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
        }));
        let out = DispatchMapStepKind.run_map(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap().outcome;
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].ok);
        assert_eq!(results[0].retried, 0, "no retry budget — never retried");
        assert_eq!(calls.get(), 1, "exactly one call — the historical no-retry-on-error behavior");
    }

    #[test]
    fn dispatch_map_retry_on_error_out_of_range_is_a_loud_config_error() {
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "retry_on_error": u64::from(u32::MAX) + 1,
        }));
        let err = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err();
        assert!(err.to_string().contains("retry_on_error"), "{err}");
    }

    #[test]
    fn dispatch_map_retry_on_empty_out_of_range_is_a_loud_config_error() {
        // (#1442 gate CONSIDER) A `retry_on_empty` beyond u32's range must be
        // a LOUD config error at step-run time — never silently coerced into
        // ~4 billion re-dispatches (the prior `unwrap_or(u32::MAX)` behavior).
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "retry_on_empty": u64::from(u32::MAX) + 1,
        }));
        let err = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("retry_on_empty"), "{msg}");
        assert!(msg.contains("exceeds the maximum"), "{msg}");
    }

    #[test]
    fn dispatch_map_retry_on_empty_wrong_type_is_a_loud_config_error() {
        // (#1442 gate CONSIDER) A present-but-non-integer `retry_on_empty`
        // (here a string) is loud too — the same "invalid key is loud"
        // doctrine, not a silent fall-through to the default 0.
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "retry_on_empty": "lots",
        }));
        let err = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("retry_on_empty"), "{msg}");
        assert!(msg.contains("non-negative integer"), "{msg}");
    }

    // ── (#1442) per-item served_model + wall_ms telemetry ───────────────

    /// A hosted reply that pauses `delay_ms` before returning — the test seam
    /// for a per-item `wall_ms` a real dispatch would earn. `served` names the
    /// endpoint-reported model (`None` reproduces an endpoint that omits it).
    fn install_hosted_delayed(delay_ms: u64, served: Option<&'static str>, total: Option<u64>) {
        install_hosted_override(move |_req| {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            Ok(crate::single_shot::SingleShotReply {
                content: "flag".to_string(),
                model: served.map(str::to_string),
                counts: darkmux_trajectory::UsageCounts { total, ..Default::default() },
            })
        });
    }

    #[test]
    #[serial_test::serial] // mutates the remote-budget env var
    fn dispatch_map_hosted_emits_liveness_bookends_carrying_the_endpoint() {
        // (#1607) THE conformance test for contract #2: "any production code
        // path that performs model work emits `dispatch.start` and a terminal
        // ... new vocabularies supplement, never replace."
        //
        // `dispatch.map` emitted only its own `step result` vocabulary, so the
        // per-seat `task-<id>` sessions it mints — the review's probe and
        // verify seats — carried token records with no record anywhere naming
        // WHERE they ran. `payload.endpoint` on these bookends is the ONLY
        // thing the savings hero reads to call a session cloud; without them
        // hosted spend is unattributable (229,034 tokens on one machine in one
        // day, reported as local until the consumer learned to say "unknown").
        //
        // Contract violations recur — this is the second (#1272 was the
        // first) — so this asserts the SHAPE, not one field.
        clear_hosted_override();
        install_hosted_delayed(1, None, Some(42));
        let s = map_step(json!({
            "model": "gpt-4o",
            "user_template": "check {item}",
            "collection": ["a", "b"],
            "endpoint": { "url": "https://example.cognitiveservices.azure.com" },
        }));
        // The bookends ride the STREAMING seam, so drive `run_streaming` with a
        // live emitter — the same shape the scheduler supplies in production.
        // The batched `run()` path deliberately emits none (see StepBookend).
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = StepRunCtx::new(crate::test_run(), 
            Some(tx),
            None,
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );
        DispatchMapStepKind
            .run(&s, &empty_task(), &BTreeMap::new(), &ctx)
            .unwrap();
        drop(ctx);
        clear_hosted_override();

        let emitted: Vec<darkmux_flow::FlowRecord> = rx
            .into_iter()
            .filter_map(|sig| match sig {
                crate::step_kinds::WaveSignal::Record(r) => Some(r),
                _ => None,
            })
            .collect();
        let bookends: Vec<&darkmux_flow::FlowRecord> = emitted.iter().filter(|r| r.action.bookend().is_some()).collect();
        let actions: Vec<&str> = bookends.iter().map(|r| r.action.as_str()).collect();
        // Each item is one execution: start, then its own terminal, before
        // the next item's start. The step itself has no bookend of its own
        // (the scheduler's `step.start`/`step.complete` cover it), and the
        // drop guard never double-emits alongside the clean close.
        assert_eq!(
            actions,
            ["dispatch.start", "dispatch.complete", "dispatch.start", "dispatch.complete"],
            "a 2-item map is 2 executions, each opened and closed in turn"
        );
        let id_of = |r: &darkmux_flow::FlowRecord| r.execution_id.clone().expect("a bookend names its execution");
        assert_eq!(id_of(bookends[0]), id_of(bookends[1]), "an item's start and terminal name one execution");
        assert_eq!(id_of(bookends[2]), id_of(bookends[3]));
        assert_ne!(id_of(bookends[0]), id_of(bookends[2]), "two items are two executions");
        // Each item's bookends name the item and, on the start, the endpoint the
        // call goes to (the live view keys on the start).
        for (bookend, index) in [(bookends[0], 0), (bookends[1], 0), (bookends[2], 1), (bookends[3], 1)] {
            assert_eq!(bookend.payload_json()["item_index"].as_u64(), Some(index), "{bookend:?}");
        }
        for start in [bookends[0], bookends[2]] {
            assert_eq!(
                start.payload_json()["endpoint"],
                "azure:example.cognitiveservices.azure.com/gpt-4o",
                "the START names where the seat runs, before any terminal exists"
            );
        }
        for terminal in [bookends[1], bookends[3]] {
            let payload = terminal.payload_json();
            assert_eq!(
                payload["endpoint"], "azure:example.cognitiveservices.azure.com/gpt-4o",
                "the terminal names WHERE the seat ran, in the one format the viewer parses"
            );
            assert!(payload.get("remote_tokens").is_none(), "tokens are on the usage record, not the terminal: {payload}");
            assert_eq!(payload["total_turns"].as_u64(), Some(1), "an item that produced a reply is one turn");
            assert!(payload["wall_ms"].is_u64(), "and carries its own wall clock: {payload}");
            assert_eq!(
                terminal.session_id.as_deref(),
                Some(darkmux_types::session_id::SessionId::task(crate::test_run(), "t1").wire().as_str()),
                "SAME session as the seat's token records: that join is the whole point"
            );
        }
        // Each item's usage record names the same execution its bookends do.
        let usage: Vec<_> = emitted.iter().filter(|r| r.action == darkmux_flow::FlowAction::TelemetryTokens).collect();
        assert_eq!(usage.len(), 2);
        assert_eq!(id_of(usage[0]), id_of(bookends[0]));
        assert_eq!(id_of(usage[1]), id_of(bookends[2]));
    }

    // ── the run bracket around N executions (contract 8) ────────────────

    /// The records of a 3-item hosted map inside the run bracket a mission
    /// launch puts around it (`run.start` ... `run.complete`/`run.error`,
    /// through the same `BookendGuard`), in the order the one channel saw
    /// them. `item_reply` decides each item's reply by index: `Ok`, `Err`,
    /// or a panic.
    fn bracketed_map_records(
        item_reply: impl Fn(usize) -> Result<crate::single_shot::SingleShotReply> + 'static,
        close_as: darkmux_flow::FlowAction,
    ) -> Vec<darkmux_flow::FlowRecord> {
        let run_session = darkmux_types::session_id::SessionId::run(crate::test_run());
        let run_record = |action| {
            darkmux_flow::FlowRecord::for_session(
                &run_session,
                darkmux_flow::Level::Info,
                darkmux_flow::Category::Work,
                darkmux_flow::Stage::Dispatch,
                action,
                "bracket-test",
            )
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let record_tx = tx.clone();
        let mut sink = move |r: darkmux_flow::FlowRecord| {
            let _ = record_tx.send(crate::step_kinds::WaveSignal::Record(r));
        };
        let calls = std::cell::Cell::new(0usize);
        clear_hosted_override();
        install_hosted_override(move |_req| {
            let n = calls.get();
            calls.set(n + 1);
            item_reply(n)
        });
        let s = map_step(json!({
            "model": "gpt-4o",
            "user_template": "check {item}",
            "collection": ["a", "b", "c"],
            "endpoint": { "url": "https://example.cognitiveservices.azure.com" },
        }));
        let ctx = StepRunCtx::new(
            crate::test_run(),
            Some(tx),
            None,
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = darkmux_flow::BookendGuard::new(&mut sink, |_id, _kind| run_record(darkmux_flow::FlowAction::RunError));
            guard.open("run", "run", run_record(darkmux_flow::FlowAction::RunStart));
            DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &ctx).unwrap();
            guard.close("run", run_record(close_as));
        }));
        std::panic::set_hook(prev_hook);
        clear_hosted_override();
        drop(unwound);
        // Every sender must be gone for the channel to end.
        drop(sink);
        drop(ctx);
        rx.into_iter()
            .filter_map(|sig| match sig {
                crate::step_kinds::WaveSignal::Record(r) => Some(r),
                _ => None,
            })
            .filter(|r| r.action.bookend().is_some())
            .collect()
    }

    /// The whole promise of the run bracket, over the records `bracketed_map_records`
    /// saw: `run.start`, then N x (`dispatch.start`, its own terminal), then
    /// one run terminal, and every execution named by a distinct id. Returns
    /// the terminals, in order.
    fn assert_run_bracket(records: &[darkmux_flow::FlowRecord], executions: usize, run_terminal: darkmux_flow::FlowAction) -> Vec<darkmux_flow::FlowAction> {
        let actions: Vec<darkmux_flow::FlowAction> = records.iter().map(|r| r.action.clone()).collect();
        assert_eq!(actions.first(), Some(&darkmux_flow::FlowAction::RunStart), "{actions:?}");
        assert_eq!(actions.last(), Some(&run_terminal), "{actions:?}");
        let inner = &records[1..records.len() - 1];
        assert_eq!(inner.len(), executions * 2, "{actions:?}");
        let mut seen = std::collections::HashSet::new();
        let mut terminals = Vec::new();
        for pair in inner.chunks(2) {
            assert_eq!(pair[0].action, darkmux_flow::FlowAction::DispatchStart, "{actions:?}");
            assert!(pair[1].action.bookend().is_some_and(|b| b.edge.is_terminal()), "{actions:?}");
            let id = pair[0].execution_id.clone().expect("an execution bookend names its execution");
            assert_eq!(pair[1].execution_id.as_ref(), Some(&id), "a pair names one execution: {actions:?}");
            assert!(seen.insert(id), "two executions share an id: {actions:?}");
            terminals.push(pair[1].action.clone());
        }
        assert!(
            records[0].execution_id.is_none() && records[records.len() - 1].execution_id.is_none(),
            "the run grain names no execution"
        );
        terminals
    }

    #[test]
    #[serial_test::serial]
    fn a_map_inside_a_run_is_run_start_then_n_executions_then_run_complete() {
        use darkmux_flow::FlowAction as A;
        let records = bracketed_map_records(|_| Ok(hosted_reply(Some(7))), A::RunComplete);
        let terminals = assert_run_bracket(&records, 3, A::RunComplete);
        assert_eq!(terminals, [A::DispatchComplete, A::DispatchComplete, A::DispatchComplete]);
    }

    /// An item that errors is an errored execution; its siblings and the run
    /// still complete (per-item isolation).
    #[test]
    #[serial_test::serial]
    fn an_erroring_item_is_one_errored_execution_inside_a_completed_run() {
        use darkmux_flow::FlowAction as A;
        let records = bracketed_map_records(
            |n| if n == 1 { Err(anyhow!("upstream 500")) } else { Ok(hosted_reply(Some(7))) },
            A::RunComplete,
        );
        let terminals = assert_run_bracket(&records, 3, A::RunComplete);
        assert_eq!(terminals, [A::DispatchComplete, A::DispatchError, A::DispatchComplete]);
    }

    /// A panic mid-item: the item's own guard closes ITS execution as
    /// `dispatch.error` and the run's guard closes the run as `run.error`,
    /// with nothing left open and no later item started.
    #[test]
    #[serial_test::serial]
    fn a_panic_mid_item_closes_its_execution_and_then_the_run_by_raii() {
        use darkmux_flow::FlowAction as A;
        let records = bracketed_map_records(
            |n| if n == 1 { panic!("simulated mid-item panic") } else { Ok(hosted_reply(Some(7))) },
            A::RunComplete,
        );
        let terminals = assert_run_bracket(&records, 2, A::RunError);
        assert_eq!(terminals, [A::DispatchComplete, A::DispatchError]);
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL
    fn dispatch_map_local_streaming_bookends_carry_the_namespaced_identifier() {
        // (MUST FIX 5) The LOCAL twin of the hosted bookend test above —
        // `dispatch.map`'s bookends, like `dispatch.single_shot`'s, are only
        // observable through the streaming channel (`StepBookend::new` only
        // emits through a `ctx`; the batched `run()` path is inert for
        // them). Reverting `run_map`'s `ExecutionBookends` `model:
        // wire_model.as_ref()` back to the bare `model` leaves
        // the per-item dispatches themselves succeeding (the mock only
        // inspects the chat body) while the bookends silently go back to
        // naming the wrong model.
        use httpmock::prelude::*;
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .json_body_partial(r#"{"model": "darkmux:qwen3-4b"}"#);
            then.status(200).header("content-type", "application/json").json_body(json!({
                "id": "mock-1",
                "object": "chat.completion",
                "created": 0,
                "model": "qwen3-4b",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
            }));
        });

        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev = std::env::var(url_key).ok();
        unsafe {
            std::env::set_var(url_key, server.base_url());
        }

        let s = map_step(json!({
            "model": "qwen3-4b",
            "user_template": "check {item}",
            "collection": ["a"],
        }));
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = StepRunCtx::new(crate::test_run(), 
            Some(tx),
            None,
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );
        let result = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &ctx);
        drop(ctx);

        unsafe {
            match prev {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }

        result.expect("the mock only answers the namespaced body");
        mock.assert_hits(1);

        let emitted: Vec<darkmux_flow::FlowRecord> = rx
            .into_iter()
            .filter_map(|sig| match sig {
                crate::step_kinds::WaveSignal::Record(r) => Some(r),
                _ => None,
            })
            .collect();
        let bookends: Vec<&darkmux_flow::FlowRecord> =
            emitted.iter().filter(|r| matches!(r.action, darkmux_flow::FlowAction::DispatchStart | darkmux_flow::FlowAction::DispatchComplete | darkmux_flow::FlowAction::DispatchError)).collect();
        assert_eq!(bookends.len(), 2, "expected a start and a complete bookend: {emitted:?}");
        for rec in &bookends {
            assert_eq!(
                rec.model.as_deref(),
                Some("darkmux:qwen3-4b"),
                "bookend record must carry the namespaced identifier the map step actually \
                 addressed: {rec:?}"
            );
        }
    }

    // ── (#2344) dispatch.map — session-presence heartbeat ────────────────

    /// A minimal fake Redis peer that ACKS everything: it completes the two
    /// `CLIENT SETINFO` handshake commands redis-rs pipelines on connect,
    /// then answers `+OK\r\n` to whatever real command follows, and records
    /// the raw bytes of every command it saw into `log` (in connection
    /// order) so the test can inspect what was actually sent.
    ///
    /// Unlike `darkmux-flow`'s own `spawn_silent_redis_peer` (which never
    /// reads and never replies to the real command, `pub(crate)` there and
    /// unreachable from this crate anyway), this one has to actually ANSWER
    /// — `SessionEmitter`'s beat thread and `stop()` both hang waiting on a
    /// reply otherwise, which would make this a liveness test, not a
    /// presence-content test.
    ///
    /// **Known limit (fresh-review finding), left as a comment rather than
    /// engineered away:** each `read` is logged as ONE entry, and the
    /// assertions below match a substring PAIR (`"SET"` and the key) within
    /// a single entry. A command split across two reads would therefore
    /// read as no match and fail the test spuriously. The commands in
    /// question are ~161 bytes over loopback into a 4 KiB buffer, which the
    /// kernel does not split in practice — 20 of 20 runs under heavy load
    /// saw one read per command — so the fix (accumulate per connection and
    /// parse RESP) would buy nothing but more test machinery. If this ever
    /// flakes, that is the reason and that is the fix.
    fn spawn_acking_recording_redis_server(log: Arc<Mutex<Vec<String>>>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let log = Arc::clone(&log);
                std::thread::spawn(move || {
                    use std::io::{Read, Write};
                    // Two `+OK` replies satisfy the two ignored `CLIENT
                    // SETINFO` commands redis-rs 0.27's
                    // `connection_setup_pipeline` sends before any real
                    // command — same shape `spawn_silent_redis_peer` uses.
                    let _ = stream.write_all(b"+OK\r\n+OK\r\n");
                    let _ = stream.flush();
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(2000)));
                    let mut buf = [0u8; 4096];
                    // Each darkmux-flow call site opens its own fresh
                    // connection per command, so one real command (plus
                    // possibly the client closing right after) is all this
                    // connection ever sees.
                    for _ in 0..4 {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                log.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).into_owned());
                                let _ = stream.write_all(b"+OK\r\n");
                                let _ = stream.flush();
                            }
                        }
                    }
                });
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        port
    }

    #[test]
    #[serial_test::serial] // mutates the DARKMUX_REDIS_URL env var
    fn dispatch_map_writes_and_releases_a_session_presence_beat() {
        // (#2344) THE conformance test for the presence half of contract #2:
        // `dispatch.map` already opens/closes its liveness BOOKENDS (#1607,
        // the test above), but bookends are terminal-only records — they say
        // "this started" and "this ended", never "this is still running".
        // The live fleet view keys "running now" on the SEPARATE
        // `darkmux:session-presence:<sid>` heartbeat
        // (`darkmux-flow::session_presence`), which — before this fix —
        // only the container dispatch path ever wrote
        // (`dispatch_internal.rs`'s `session_emitter`). A `dispatch.map`
        // step doing real in-process model work (the per-item loop) never
        // wrote one, so a running map read as invisible on the live view
        // exactly like #2344 describes.
        //
        // Deliberately does NOT assert the final `DEL` lands: `stop()`'s DEL
        // is documented as best-effort even in production ("a Redis blip on
        // the final DEL just means the key ages out via TTL instead" —
        // `session_presence.rs`), and under real CPU contention this test's
        // fake peer measurably reproduces exactly that blip against
        // `open_redis_connection_bounded`'s tight 500ms connect budget —
        // asserting it deterministically would test the machine's load, not
        // this fix. What this test asserts instead is deterministic and is
        // the property that actually matters (the task's own framing: "a
        // beat that never stops is worse than no beat"): (1) `claim_edge`'s
        // `SET NX` — the FIRST thing `stop()` does after joining the beat
        // thread — proves `.stop()` was reached and the thread was joined;
        // (2) waiting past one full beat interval and seeing no SECOND `SET`
        // proves the beat thread actually stopped ticking rather than
        // continuing to refresh the key forever.
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let port = spawn_acking_recording_redis_server(Arc::clone(&log));

        let prev = std::env::var("DARKMUX_REDIS_URL").ok();
        unsafe {
            std::env::set_var("DARKMUX_REDIS_URL", format!("redis://127.0.0.1:{port}"));
        }

        // A dispatch override that sleeps briefly before returning — the
        // same trick `install_hosted_delayed` uses for `wall_ms` — gives the
        // presence beat thread (spawned before the item loop starts) a
        // window to actually get scheduled and complete its first `SET`
        // before the loop finishes and `run_map` calls `.stop()`. Without
        // this, a same-thread synchronous test can race the beat thread to
        // zero.
        let ovr: MapDispatchOverride = Arc::new(move |_call: &OverrideDispatchCall<'_>| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            Ok(crate::single_shot::SingleShotReply {
                content: "ok".to_string(),
                model: None,
                counts: darkmux_trajectory::UsageCounts { total: Some(5), ..Default::default() },
            })
        });

        let s = map_step(json!({
            "model": "qwen3.6-35b",
            "user_template": "check {item}",
            "collection": ["a"],
        }));
        let ctx = StepRunCtx::new(crate::test_run(), None, Some(ovr), Arc::new(crate::step_kinds::ArtifactBus::new()));
        DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &ctx).unwrap();

        // (2) — see doc above: past one full beat interval, a still-ticking
        // thread would have fired a second SET by now.
        std::thread::sleep(std::time::Duration::from_secs(
            darkmux_flow::session_presence::DEFAULT_BEAT_INTERVAL_SECS + 1,
        ));

        match prev {
            Some(v) => unsafe { std::env::set_var("DARKMUX_REDIS_URL", v) },
            None => unsafe { std::env::remove_var("DARKMUX_REDIS_URL") },
        }

        let expected_key = format!(
            "darkmux:session-presence:{}",
            darkmux_types::session_id::SessionId::task(crate::test_run(), "t1").wire()
        );
        let entries = log.lock().unwrap().clone();
        let set_count =
            entries.iter().filter(|e| e.contains("SET") && e.contains(&expected_key)).count();
        let saw_claim_edge = entries.iter().any(|e| e.contains(&format!("edge-claim:session-end:{}", darkmux_types::session_id::SessionId::task(crate::test_run(), "t1"))));
        assert_eq!(
            set_count, 1,
            "expected exactly ONE session-presence SET beat for {expected_key} (fired once \
             while the map's item loop ran) and no more after — a second one after the loop \
             finished means the heartbeat thread was never told to stop; saw {entries:?}"
        );
        assert!(
            saw_claim_edge,
            "expected `stop()`'s session-end edge claim — the first thing `stop()` does after \
             joining the beat thread — proving the release path was actually reached, not just \
             the beat's start; saw {entries:?}"
        );
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_REDIS_URL and DARKMUX_LMSTUDIO_URL
    fn dispatch_map_local_session_beat_carries_the_namespaced_identifier() {
        // (MUST FIX 5) The content-capturing twin of the test above (which
        // only pins that a beat fires and stops) and the LOCAL twin of
        // `dispatch_single_shot_local_session_beat_carries_the_namespaced_
        // identifier` below — routed through a REAL LMStudio mock (rather
        // than the `MapDispatchOverride` seam the test above uses) so the
        // namespaced-wire-body check and the beat-content check both apply
        // to the SAME dispatch. Reverting `dispatch.map`'s
        // `spawn_session_emitter` call back to the bare `model` leaves the
        // per-item dispatch succeeding while the beat silently goes back to
        // naming the wrong model.
        use httpmock::prelude::*;
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .json_body_partial(r#"{"model": "darkmux:qwen3-4b"}"#);
            then.status(200).header("content-type", "application/json").json_body(json!({
                "id": "mock-1",
                "object": "chat.completion",
                "created": 0,
                "model": "qwen3-4b",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
            }));
        });

        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let redis_port = spawn_acking_recording_redis_server(Arc::clone(&log));

        let lms_key = "DARKMUX_LMSTUDIO_URL";
        let prev_lms = std::env::var(lms_key).ok();
        let redis_key = "DARKMUX_REDIS_URL";
        let prev_redis = std::env::var(redis_key).ok();
        unsafe {
            std::env::set_var(lms_key, server.base_url());
            std::env::set_var(redis_key, format!("redis://127.0.0.1:{redis_port}"));
        }

        let s = map_step(json!({
            "model": "qwen3-4b",
            "user_template": "check {item}",
            "collection": ["a"],
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test());

        unsafe {
            match prev_lms {
                Some(v) => std::env::set_var(lms_key, v),
                None => std::env::remove_var(lms_key),
            }
            match prev_redis {
                Some(v) => std::env::set_var(redis_key, v),
                None => std::env::remove_var(redis_key),
            }
        }

        out.expect("the mock only answers the namespaced body, so a bare-key dispatch fails");
        mock.assert_hits(1);

        let entries = log.lock().unwrap().clone();
        let beat_entry = entries
            .iter()
            .find(|e| e.contains("SET") && e.contains("darkmux:session-presence:"))
            .unwrap_or_else(|| panic!("expected a session-presence SET beat; saw {entries:?}"));
        assert!(
            beat_entry.contains("darkmux:qwen3-4b"),
            "the session-presence beat must name the namespaced identifier the map step \
             actually addressed, not the bare `qwen3-4b` config value: {beat_entry:?}"
        );
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_REDIS_URL
    fn dispatch_single_shot_writes_a_session_presence_beat_and_its_liveness_bookends() {
        // (#2344, fresh-review blocker) `dispatch.single_shot` was the WORST
        // of the uncovered paths, and it sat nine hundred lines above the
        // `dispatch.map` fix in this same file: it performs real model work
        // (one chat completion) and emitted neither a presence beat NOR the
        // contract-#2 bookends — only its own `step result` vocabulary, so a
        // seat had tokens and a model with no start, no terminal, and no
        // liveness anywhere.
        //
        // Driven against TWO fakes and zero real anything: an httpmock HTTP
        // server standing in for the hosted endpoint (the same in-process
        // mode `tests/mock_single_shot_proof.rs` uses, reached over the real
        // hardened curl path) and the acking Redis peer above. No LMStudio,
        // no Docker, no model.
        use httpmock::prelude::*;
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).header("content-type", "application/json").json_body(json!({
                "id": "mock-1",
                "object": "chat.completion",
                "created": 0,
                "model": "gpt-5.1",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10 },
            }));
        });

        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let port = spawn_acking_recording_redis_server(Arc::clone(&log));
        let prev = std::env::var("DARKMUX_REDIS_URL").ok();
        unsafe {
            std::env::set_var("DARKMUX_REDIS_URL", format!("redis://127.0.0.1:{port}"));
        }

        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({
                "model": "gpt-5.1",
                "user": "hello",
                "endpoint": { "url": format!("{}/v1", server.base_url()) },
            }),
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = StepRunCtx::new(crate::test_run(), 
            Some(tx),
            None,
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );
        let out = DispatchSingleShotStepKind
            .run(&s, &empty_task(), &BTreeMap::new(), &ctx)
            .expect("the mock endpoint answers, so the step completes");
        drop(ctx);

        match prev {
            Some(v) => unsafe { std::env::set_var("DARKMUX_REDIS_URL", v) },
            None => unsafe { std::env::remove_var("DARKMUX_REDIS_URL") },
        }

        assert_eq!(out.output, "ok", "the reply came back over the mock HTTP server");

        // Presence — same two deterministic properties the map test asserts,
        // and for the same reasons (see its doc): the beat fired, and
        // `stop()` was genuinely reached (its session-end pre-claim is the
        // first thing it does after joining the beat thread).
        let expected_key = format!(
            "darkmux:session-presence:{}",
            darkmux_types::session_id::SessionId::task(crate::test_run(), "t1").wire()
        );
        let entries = log.lock().unwrap().clone();
        assert!(
            entries.iter().any(|e| e.contains("SET") && e.contains(&expected_key)),
            "a hosted `dispatch.single_shot` must beat while it is generating — expected a \
             session-presence SET for {expected_key}; saw {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e.contains(&format!("edge-claim:session-end:{}", darkmux_types::session_id::SessionId::task(crate::test_run(), "t1")))),
            "expected `stop()`'s session-end edge claim, proving the beat was released rather \
             than left to TTL out; saw {entries:?}"
        );

        // Bookends — the half this kind never had at all.
        let records: Vec<darkmux_flow::FlowRecord> = rx
            .into_iter()
            .filter_map(|sig| match sig {
                crate::step_kinds::WaveSignal::Record(r) => Some(r),
                _ => None,
            })
            .collect();
        let actions: Vec<darkmux_flow::FlowAction> = records.iter().map(|r| r.action.clone()).collect();
        // Every `dispatch.complete` states its turn count and its own accounting: this one
        // call is one turn, with the split its usage record carries.
        let complete = records.iter().find(|r| r.action == darkmux_flow::FlowAction::DispatchComplete).expect("a terminal").payload_json();
        assert_eq!(complete["total_turns"].as_u64(), Some(1), "one call is one turn");
        assert!(complete["wall_ms"].is_u64(), "the call's own wall clock: {complete}");
        assert_eq!(complete["prompt_tokens"].as_u64(), Some(5));
        assert_eq!(complete["completion_tokens"].as_u64(), Some(5));
        assert_eq!(
            actions.iter().filter(|a| **a == darkmux_flow::FlowAction::DispatchStart).count(),
            1,
            "exactly one liveness start; got {actions:?}"
        );
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, darkmux_flow::FlowAction::DispatchComplete | darkmux_flow::FlowAction::DispatchError))
                .count(),
            1,
            "exactly one terminal per open — the Drop guard must not double-emit alongside \
             the clean close; got {actions:?}"
        );
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_REDIS_URL and DARKMUX_LMSTUDIO_URL
    fn dispatch_single_shot_local_session_beat_carries_the_namespaced_identifier() {
        // (MUST FIX 5) The one record site the flow-record-only assertions
        // elsewhere in this file structurally cannot reach: the session-
        // presence BEAT is not a `FlowRecord` at all — it rides its own
        // Redis SET, invisible to both the batched `run()` output and the
        // streaming channel. The ONLY way to pin what model it actually
        // names is to capture the real bytes on the wire, the same way
        // `dispatch_single_shot_writes_a_session_presence_beat_and_its_
        // liveness_bookends` above already does with
        // `spawn_acking_recording_redis_server` — reused here unchanged,
        // just against a LOCAL (non-hosted) dispatch instead of a hosted
        // one, since the namespaced-identifier split (#2570) only exists on
        // the local arm. Reverting `spawn_session_emitter`'s `Some(wire_
        // model.to_string())` argument back to the bare `model` leaves the
        // dispatch itself succeeding (the LMStudio mock only inspects the
        // CHAT body, never the presence beat) while the live fleet view
        // silently goes back to showing the wrong model for a running local
        // seat.
        use httpmock::prelude::*;
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .json_body_partial(r#"{"model": "darkmux:qwen3-4b"}"#);
            then.status(200).header("content-type", "application/json").json_body(json!({
                "id": "mock-1",
                "object": "chat.completion",
                "created": 0,
                "model": "qwen3-4b",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
            }));
        });

        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let redis_port = spawn_acking_recording_redis_server(Arc::clone(&log));

        let lms_key = "DARKMUX_LMSTUDIO_URL";
        let prev_lms = std::env::var(lms_key).ok();
        let redis_key = "DARKMUX_REDIS_URL";
        let prev_redis = std::env::var(redis_key).ok();
        unsafe {
            std::env::set_var(lms_key, server.base_url());
            std::env::set_var(redis_key, format!("redis://127.0.0.1:{redis_port}"));
        }

        let s = step("s1", "dispatch.single_shot", json!({ "model": "qwen3-4b", "user": "hi" }));
        let out = DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test());

        unsafe {
            match prev_lms {
                Some(v) => std::env::set_var(lms_key, v),
                None => std::env::remove_var(lms_key),
            }
            match prev_redis {
                Some(v) => std::env::set_var(redis_key, v),
                None => std::env::remove_var(redis_key),
            }
        }

        out.expect("the mock only answers the namespaced body, so a bare-key dispatch fails");
        mock.assert_hits(1);

        let entries = log.lock().unwrap().clone();
        let beat_entry = entries
            .iter()
            .find(|e| e.contains("SET") && e.contains("darkmux:session-presence:"))
            .unwrap_or_else(|| panic!("expected a session-presence SET beat; saw {entries:?}"));
        assert!(
            beat_entry.contains("darkmux:qwen3-4b"),
            "the session-presence beat must name the namespaced identifier the dispatch \
             actually addressed, not the bare `qwen3-4b` config value: {beat_entry:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_local_bookend_carries_no_endpoint() {
        // The no-op-when-local half: a local seat's terminal must be
        // byte-identical to one from a build with no endpoint concept at all,
        // or every local dispatch starts reporting as cloud.
        clear_hosted_override();
        let s = map_step(json!({
            "model": "qwen3.6-35b",
            "user_template": "check {item}",
            "collection": [],
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        for r in &out.flow_records {
            if r.payload.is_some() {
                let p = r.payload_json();
                assert!(
                    p.get("endpoint").is_none(),
                    "a local map must never stamp an endpoint: {p}"
                );
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_hosted_item_surfaces_served_model_and_nonzero_wall_ms() {
        // The endpoint reports it served "served-model-x" and the call takes a
        // real (seam-controlled) ~15ms — the HOSTED item must surface BOTH the
        // served model verbatim and a nonzero cumulative wall, in its result,
        // its per-item flow record, AND (wall) the step aggregate's sum.
        clear_hosted_override();
        install_hosted_delayed(15, Some("served-model-x"), Some(10));
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].served_model.as_deref(),
            Some("served-model-x"),
            "the endpoint-reported served model passes through verbatim"
        );
        assert!(
            results[0].wall_ms >= 1,
            "a real ~15ms dispatch earns a nonzero wall_ms, got {}",
            results[0].wall_ms
        );
        // Per-item flow record (index 0) carries the same telemetry; the
        // aggregate (last) carries the SUMMED wall.
        let item_payload = out.flow_records[0].payload_json();
        assert_eq!(item_payload["served_model"], "served-model-x");
        assert!(item_payload["wall_ms"].as_u64().unwrap() >= 1);
        let agg = out.flow_records.last().unwrap().payload_json();
        assert_eq!(
            agg["total_wall_ms"].as_u64().unwrap(),
            results[0].wall_ms,
            "the aggregate's total_wall_ms is the sum of the per-item walls"
        );
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_hosted_item_served_model_is_none_when_the_endpoint_omits_it() {
        // An endpoint that omits `model` yields an honest `None` served_model —
        // never a fabricated empty string and never the requested model echoed
        // back as if served.
        clear_hosted_override();
        install_hosted_delayed(0, None, Some(10));
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].ok, "it dispatched");
        assert_eq!(
            results[0].served_model, None,
            "an omitting endpoint yields honest None — never a fabricated or echoed value"
        );
        assert_ne!(
            results[0].served_model.as_deref(),
            Some("gpt-5.1"),
            "the requested model is never echoed into served_model"
        );
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_local_item_served_model_is_none_by_construction() {
        // The LOCAL arm never reads the response's echoed `model` — `lms ps` is
        // the only ground truth for a local dispatch — so `served_model` is
        // `None` by construction. Point the local dialect at an unroutable
        // endpoint so each item errors; the error path still carries the
        // measured wall and a `None` served model.
        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev = std::env::var(url_key).ok();
        unsafe {
            std::env::set_var(url_key, "http://127.0.0.1:1");
        }
        let s = map_step(json!({
            "model": "m",
            "user_template": "check {item}",
            "collection": ["a", "b"],
            "timeout_seconds": 1,
        }));
        let out = DispatchMapStepKind.run_map(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap().outcome;
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 2);
        assert!(
            results.iter().all(|r| r.served_model.is_none()),
            "a local item never surfaces a served model, even one the response echoed"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_retry_accumulates_wall_across_attempts() {
        // Two attempts each pause a seam-controlled 20ms (empty, then content),
        // retry_on_empty=1 → BOTH fire. `total_tokens` Some(120) independently
        // proves two calls ran; `wall_ms` is their CUMULATIVE sum (>= the 40ms
        // floor of two 20ms sleeps, minus <2ms of millis truncation) — the same
        // per-attempt accumulation `total_tokens` already uses.
        clear_hosted_override();
        // A scripted seam that also sleeps 20ms per call: empty (bills 50),
        // then content (bills 70).
        {
            let idx = std::cell::Cell::new(0usize);
            let script: std::rc::Rc<Vec<(&'static str, Option<u64>)>> =
                std::rc::Rc::new(vec![("", Some(50)), ("flag", Some(70))]);
            install_hosted_override(move |_req| {
                std::thread::sleep(std::time::Duration::from_millis(20));
                let i = idx.get().min(script.len().saturating_sub(1));
                idx.set(idx.get() + 1);
                let (content, total) = script[i];
                Ok(crate::single_shot::SingleShotReply {
                    content: content.to_string(),
                    model: Some("served-r".to_string()),
                    counts: darkmux_trajectory::UsageCounts { total, ..Default::default() },
                })
            });
        }
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
            "retry_on_empty": 1,
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].total_tokens, Some(120), "both attempts fired (tokens summed)");
        assert!(
            results[0].wall_ms >= 30,
            "wall accumulated across BOTH 20ms attempts (>= 30ms), got {}",
            results[0].wall_ms
        );
        assert_eq!(results[0].served_model.as_deref(), Some("served-r"));
    }

    // ── (#1412, #2902 step 5) dispatch.single_shot hosted-arm budgets ───

    /// (#2902 step 5) A per-dispatch cap of 0 is no cap (the zero doctrine:
    /// a `0` on a darkmux bound means unbounded): the hosted call is
    /// ATTEMPTED (proven by the hosted arm's own error context against an
    /// unroutable port), and the error is the network's, never a budget
    /// refusal. Pre-4.0, 0 refused.
    #[test]
    #[serial_test::serial]
    fn dispatch_single_shot_hosted_arm_under_warn_calls_even_with_a_zero_dispatch_cap() {
        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({ "model": "gpt-5.1", "user": "hi", "endpoint": { "url": "http://127.0.0.1:1", "limits": { "tokens_per_dispatch": 0 } }, "timeout_seconds": 1 }),
        );
        let msg = format!("{:#}", DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err());
        assert!(msg.contains("dispatch.single_shot (hosted)"), "the call was attempted: {msg}");
        assert!(!msg.contains("budget"), "no budget refusal: {msg}");
    }

    /// (#3035) A `wait` policy needs a rolling window: a dispatch's own cap has
    /// nothing that frees room. A step whose inline endpoint says `wait` with
    /// only `tokens_per_dispatch` is refused before anything is sent, naming
    /// the policy and the window, never run as some other policy.
    #[test]
    #[serial_test::serial]
    fn dispatch_single_shot_refuses_a_wait_policy_with_no_window_before_sending() {
        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({
                "model": "gpt-5.1", "user": "hi", "timeout_seconds": 1,
                "endpoint": { "url": "http://127.0.0.1:1", "limits": { "tokens_per_dispatch": 5, "policy": "wait" } },
            }),
        );
        let msg = format!(
            "{:#}",
            DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).unwrap_err()
        );
        assert!(msg.contains("`wait`") && msg.contains("window"), "{msg}");
        assert!(!msg.contains("dispatch.single_shot (hosted)"), "nothing was sent: {msg}");
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_single_shot_local_arm_names_no_endpoint_and_so_meters_nothing() {
        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev_url = std::env::var(url_key).ok();
        unsafe {
            std::env::set_var(url_key, "http://127.0.0.1:1");
        }

        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({ "model": "some-local-model", "user": "hi", "timeout_seconds": 1 }),
        );
        let err = DispatchSingleShotStepKind
            .run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test())
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("budget"),
            "a local step that names no endpoint has no limits to be gated by: {msg}"
        );
        assert!(
            msg.contains("dispatch.single_shot (local)"),
            "expected the local-arm error context, got: {msg}"
        );

        unsafe {
            match prev_url {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }
    }

    // ─── #1530 Packet 0: every Tier 1 kind declares no ports ──────────
    //
    // The binding constraint on this packet is ZERO behavior change: adding
    // `StepKind::provides`/`StepKind::requires` must not alter what any
    // EXISTING kind does. Since every Tier 1 builtin here predates the
    // trait methods and none of their `impl StepKind` blocks override them,
    // this test is really pinning the DEFAULT (`&[]`) rather than each
    // kind's own choice — but pinning it here, against the real kinds a
    // production graph runs, is what would catch a future packet
    // accidentally giving one of these a port it shouldn't have.
    #[test]
    fn tier1_kinds_declare_no_ports_beyond_the_untyped_text_producers() {
        let kinds: Vec<Arc<dyn StepKind>> = vec![
            Arc::new(DispatchInternalStepKind),
            Arc::new(DispatchSingleShotStepKind),
            Arc::new(DispatchMapStepKind),
            Arc::new(ProceduralShellStepKind),
            Arc::new(ProceduralNoopStepKind),
        ];
        for kind in kinds {
            // (#2312) The untyped producers provide "text" and nothing else.
            let untyped = matches!(kind.id(), "dispatch.internal" | "procedural.shell");
            let provided: Vec<&str> = kind.provides().iter().map(|p| p.name).collect();
            assert_eq!(
                provided,
                if untyped { vec!["text"] } else { vec![] },
                "`{}` declares the wrong `provides` ports",
                kind.id()
            );
            assert!(
                kind.requires().is_empty(),
                "`{}` should declare no `requires` ports (Tier 1 backward-compat)",
                kind.id()
            );
        }
    }

    /// (#2329 review) The placement must resolve the profile exactly as the
    /// dispatch will: explicit name > `role_profiles` binding > default.
    /// Before, the binding was skipped, so a bound role's wave leased one
    /// model while its dispatch loaded another.
    #[serial_test::serial]
    #[test]
    fn placement_honors_the_role_profiles_binding_exactly_like_the_dispatch() {
        let dir = tempfile::TempDir::new().unwrap();
        let reg = dir.path().join("profiles.json");
        std::fs::write(
            &reg,
            serde_json::json!({
                "default_profile": "p",
                "profiles": {
                    "p": {"models": [{"id": "m-default", "n_ctx": 4096, "role": "primary"}]},
                    "q": {"models": [{"id": "m-mapped", "n_ctx": 8192, "role": "primary"}]}
                }
            })
            .to_string(),
        )
        .unwrap();
        let cfg = reg.to_str().unwrap();
        let pick = |name: Option<&str>, mapped: Option<&str>| {
            resolve_local_placement_inner_with("coder", name, mapped.map(str::to_string), Some(cfg), "step:s")
                .map(|p| p.model_key)
                .map_err(|e| match e {
                    PlacementMiss::Unmanaged(_) => "unmanaged".to_string(),
                    PlacementMiss::ResolutionFailed(r) => r,
                })
        };
        assert_eq!(pick(None, None), Ok("m-default".into()), "unbound → default_profile");
        assert_eq!(pick(None, Some("q")), Ok("m-mapped".into()), "the role_profiles binding wins over the default");
        assert_eq!(pick(Some("p"), Some("q")), Ok("m-default".into()), "an explicit profile wins over the binding");
        let err = pick(None, Some("nope")).unwrap_err();
        assert!(err.contains("nope"), "a binding to an undefined profile is a loud error naming it, never a silent fallback: {err}");
    }

    /// (#2914) The step-placement path sets the machine's utility model
    /// aside exactly like the dispatch does: a profile that still lists it
    /// places the work model, and a profile that lists only it is a loud
    /// resolution failure naming the fix, never a placement of the utility
    /// model.
    #[serial_test::serial]
    #[test]
    fn placement_never_puts_a_step_on_the_machine_utility_model() {
        let dir = tempfile::TempDir::new().unwrap();
        let reg = dir.path().join("profiles.json");
        std::fs::write(
            &reg,
            serde_json::json!({
                "default_profile": "leftover",
                "internal": { "utility": { "id": "util-4b", "n_ctx": 120000 } },
                "profiles": {
                    "leftover": {
                        "default_model": "util-4b",
                        "models": [{"id": "util-4b", "n_ctx": 16000}, {"id": "worker-35b", "n_ctx": 65536}]
                    },
                    "utility-only": {"models": [{"id": "darkmux:util-4b", "n_ctx": 16000}]}
                }
            })
            .to_string(),
        )
        .unwrap();
        let cfg = reg.to_str().unwrap();
        let pick = |name: Option<&str>| {
            resolve_local_placement_inner_with("coder", name, None, Some(cfg), "step:s")
                .map(|p| p.model_key)
                .map_err(|e| match e {
                    PlacementMiss::Unmanaged(_) => "unmanaged".to_string(),
                    PlacementMiss::ResolutionFailed(r) => r,
                })
        };
        assert_eq!(pick(None), Ok("worker-35b".into()), "the declared default is the utility model; the work model places");
        let err = pick(Some("utility-only")).unwrap_err();
        assert!(err.contains("utility model") && err.contains("internal.utility"), "names the fix: {err}");
    }

    /// (#2902 step 3) A seat whose selected model is on an UNMANAGED
    /// endpoint is the silent `Remote` miss (no local residency to plan);
    /// an undefined id is a loud resolution failure.
    #[serial_test::serial]
    #[test]
    fn placement_classifies_through_the_one_resolver() {
        let dir = tempfile::TempDir::new().unwrap();
        let reg = dir.path().join("profiles.json");
        std::fs::write(
            &reg,
            serde_json::json!({
                "default_profile": "local",
                "endpoints": { "hosted": { "url": "https://h.example/v1" } },
                "profiles": {
                    "local": {"models": [{"id": "m-local", "n_ctx": 4096}]},
                    "named": {"models": [{"id": "gpt", "endpoint": "hosted"}]},
                    "dangling": {"models": [{"id": "gpt", "endpoint": "nope"}]}
                }
            })
            .to_string(),
        )
        .unwrap();
        let cfg = reg.to_str().unwrap();
        let pick = |name: &str| {
            resolve_local_placement_inner_with("coder", Some(name), None, Some(cfg), "step:s")
                .map(|p| p.model_key)
                .map_err(|e| match e {
                    PlacementMiss::Unmanaged(_) => "unmanaged".to_string(),
                    PlacementMiss::ResolutionFailed(r) => r,
                })
        };
        assert_eq!(pick("local"), Ok("m-local".into()));
        assert_eq!(pick("named"), Err("unmanaged".into()));
        let err = pick("dangling").unwrap_err();
        assert!(err.contains("nope") && err != "unmanaged", "{err}");
    }

    // ─── (#2902 step 1a) usage conformance: one record per model call ──

    /// The mock chat server every usage-conformance test below answers from.
    /// `model` in the reply differs from the requested one on purpose, so
    /// `reported_model` is provably the RESPONSE's field, not an echo.
    fn usage_mock(server: &httpmock::MockServer, usage: bool) -> httpmock::Mock<'_> {
        use httpmock::prelude::*;
        let mut body = json!({
            "id": "mock-usage",
            "object": "chat.completion",
            "created": 0,
            "model": "served-by-mock",
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
        });
        if usage {
            body["usage"] = json!({ "prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 12 });
        }
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).header("content-type", "application/json").json_body(body);
        })
    }

    fn as_values(recs: &[darkmux_flow::FlowRecord]) -> Vec<serde_json::Value> {
        recs.iter().map(|r| serde_json::to_value(r).unwrap()).collect()
    }

    /// Runs `f` with `DARKMUX_LMSTUDIO_URL` pointed at `url`, restoring it after.
    fn with_lmstudio_url<T>(url: &str, f: impl FnOnce() -> T) -> T {
        let key = "DARKMUX_LMSTUDIO_URL";
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, url) };
        let out = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        out
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL
    fn usage_conformance_single_shot_step_local() {
        let server = httpmock::MockServer::start();
        let mock = usage_mock(&server, true);
        let s = step("s1", "dispatch.single_shot", json!({ "model": "qwen3-4b", "user": "hi" }));
        let out = with_lmstudio_url(&server.base_url(), || {
            DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test())
        })
        .expect("mock answers");
        mock.assert_hits(1);
        let recs = as_values(&out.flow_records);
        let rec = crate::usage::assert_one_usage_record(&recs, crate::usage::CallKind::SingleShot, "dispatch.single_shot (local)");
        let p = &rec["payload"];
        assert_eq!(p["requested_model"], "darkmux:qwen3-4b");
        assert_eq!(p["reported_model"], "served-by-mock");
        assert_eq!(p["endpoint"], format!("{}/v1", server.base_url()));
        // The record's own `model` is the wire identifier the call went to,
        // not the bare model key: usage attributes to the instance that answered.
        assert_eq!(rec["model"], "darkmux:qwen3-4b", "{rec}");
        assert_eq!(p["token_source"], "provider");
        assert_eq!(p["total_tokens"], 12, "provider total wins over 7+3");
        assert_eq!(rec["session_id"], darkmux_types::session_id::SessionId::task(crate::test_run(), &s.task_id).wire());
        assert_eq!(rec["handle"], "s1");
    }

    #[test]
    fn usage_conformance_single_shot_step_hosted() {
        let server = httpmock::MockServer::start();
        let mock = usage_mock(&server, true);
        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({ "model": "gpt-5.1", "user": "hi", "endpoint": { "url": format!("{}/v1", server.base_url()) } }),
        );
        let out = DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).expect("mock answers");
        mock.assert_hits(1);
        let recs = as_values(&out.flow_records);
        let rec = crate::usage::assert_one_usage_record(&recs, crate::usage::CallKind::SingleShot, "dispatch.single_shot (hosted)");
        let p = &rec["payload"];
        assert_eq!(p["requested_model"], "gpt-5.1");
        assert_eq!(p["reported_model"], "served-by-mock");
        let ep: darkmux_types::ModelEndpoint =
            serde_json::from_value(json!({ "url": format!("{}/v1", server.base_url()) })).unwrap();
        assert_eq!(
            p["endpoint"],
            crate::target::endpoint_route_label(&ep, "gpt-5.1"),
            "the same label the bookends carry"
        );
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL
    fn usage_conformance_single_shot_step_without_usage_is_absent() {
        let server = httpmock::MockServer::start();
        let _mock = usage_mock(&server, false);
        let s = step("s1", "dispatch.single_shot", json!({ "model": "qwen3-4b", "user": "hi" }));
        let out = with_lmstudio_url(&server.base_url(), || {
            DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test())
        })
        .expect("mock answers");
        let recs = as_values(&out.flow_records);
        let rec = crate::usage::assert_one_usage_record(&recs, crate::usage::CallKind::SingleShot, "dispatch.single_shot (no usage)");
        assert_eq!(rec["payload"]["token_source"], "absent");
        assert!(rec["payload"].get("total_tokens").is_none(), "no fabricated count: {rec}");
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL
    fn usage_conformance_map_item_local() {
        let server = httpmock::MockServer::start();
        let mock = usage_mock(&server, true);
        let s = map_step(json!({ "model": "qwen3-4b", "user_template": "check {item}", "collection": ["a"] }));
        let out = with_lmstudio_url(&server.base_url(), || {
            DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test())
        })
        .expect("mock answers");
        mock.assert_hits(1);
        let recs = as_values(&out.flow_records);
        let rec = crate::usage::assert_one_usage_record(&recs, crate::usage::CallKind::MapItem, "dispatch.map item (local)");
        let p = &rec["payload"];
        assert_eq!(p["requested_model"], "darkmux:qwen3-4b");
        // The response's own `model`, local items included (#2902 review).
        assert_eq!(p["reported_model"], "served-by-mock");
        assert_eq!(p["endpoint"], format!("{}/v1", server.base_url()));
        assert_eq!(p["total_tokens"], 12);
        assert_eq!(p["index"], 0);
    }

    /// Runs a LOCAL `dispatch.map` over `items` through the override seam,
    /// returning `(transport hits, usage records)`.
    fn map_usage_via_override(
        config: serde_json::Value,
        ovr: MapDispatchOverride,
        hits: Arc<Mutex<usize>>,
    ) -> (usize, Vec<serde_json::Value>) {
        let s = map_step(config);
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = StepRunCtx::new(crate::test_run(), Some(tx), Some(ovr), Arc::new(crate::step_kinds::ArtifactBus::new()));
        let _ = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &ctx);
        drop(ctx);
        let recs: Vec<serde_json::Value> = rx
            .into_iter()
            .filter_map(|sig| match sig {
                crate::step_kinds::WaveSignal::Record(r) => Some(serde_json::to_value(r).unwrap()),
                _ => None,
            })
            .filter(|r| r["action"] == "telemetry.tokens")
            .collect();
        let n = *hits.lock().unwrap();
        (n, recs)
    }

    fn scripted_reply(content: &str, total: Option<u64>) -> crate::single_shot::SingleShotReply {
        crate::single_shot::SingleShotReply {
            content: content.to_string(),
            model: Some("served-by-script".to_string()),
            counts: darkmux_trajectory::UsageCounts { total, prompt: total.map(|t| t - 2), completion: total.map(|_| 2), ..Default::default() },
        }
    }

    /// (#2902 review) An item that retries makes N model calls and must
    /// account N records, each with its own counts, summing to the item's
    /// total. The reviewer's probe shape: 3 hits, 10 tokens each.
    #[test]
    fn usage_conformance_map_item_retries_emit_one_record_per_call() {
        let hits = Arc::new(Mutex::new(0usize));
        let seen = Arc::clone(&hits);
        let ovr: MapDispatchOverride = Arc::new(move |_c: &OverrideDispatchCall<'_>| {
            *seen.lock().unwrap() += 1;
            Ok(scripted_reply("", Some(10))) // empty content: retry_on_empty fires
        });
        let (n, recs) = map_usage_via_override(
            json!({ "model": "qwen3-4b", "user_template": "x {item}", "collection": ["a"], "retry_on_empty": 2 }),
            ovr,
            hits,
        );
        assert_eq!(n, 3, "three attempts hit the transport");
        assert_eq!(recs.len(), 3, "one usage record per call: {recs:#?}");
        let sum: u64 = recs.iter().map(|r| r["payload"]["total_tokens"].as_u64().unwrap()).sum();
        assert_eq!(sum, 30);
        for r in &recs {
            assert_eq!(r["payload"]["call_kind"], "map_item");
            assert_eq!(r["payload"]["reported_model"], "served-by-script");
            assert_eq!(r["payload"]["index"], 0);
        }
    }

    /// (#2902 review, finding 3) An attempt that replied WITHOUT usage, then a
    /// later attempt that errored: the replied call still leaves an `absent`
    /// record; the errored attempt (no reply) leaves none.
    #[test]
    fn usage_conformance_map_item_replied_then_errored_keeps_the_absent_record() {
        let hits = Arc::new(Mutex::new(0usize));
        let seen = Arc::clone(&hits);
        let ovr: MapDispatchOverride = Arc::new(move |_c: &OverrideDispatchCall<'_>| {
            let mut h = seen.lock().unwrap();
            *h += 1;
            if *h == 1 {
                Ok(scripted_reply("", None))
            } else {
                anyhow::bail!("endpoint went away")
            }
        });
        let (n, recs) = map_usage_via_override(
            json!({ "model": "qwen3-4b", "user_template": "x {item}", "collection": ["a"], "retry_on_empty": 1 }),
            ovr,
            hits,
        );
        assert_eq!(n, 2);
        assert_eq!(recs.len(), 1, "{recs:#?}");
        assert_eq!(recs[0]["payload"]["token_source"], "absent");
    }

    /// An item whose only attempt errored made no completed call: no record.
    #[test]
    fn usage_conformance_map_item_with_no_reply_emits_no_record() {
        let hits = Arc::new(Mutex::new(0usize));
        let seen = Arc::clone(&hits);
        let ovr: MapDispatchOverride = Arc::new(move |_c: &OverrideDispatchCall<'_>| {
            *seen.lock().unwrap() += 1;
            anyhow::bail!("boom")
        });
        let (n, recs) = map_usage_via_override(
            json!({ "model": "qwen3-4b", "user_template": "x {item}", "collection": ["a"] }),
            ovr,
            hits,
        );
        assert_eq!(n, 1);
        assert!(recs.is_empty(), "{recs:#?}");
    }

    #[test]
    fn usage_conformance_map_item_hosted() {
        let server = httpmock::MockServer::start();
        let mock = usage_mock(&server, true);
        let ep_json = json!({ "url": format!("{}/v1", server.base_url()) });
        let s = map_step(json!({
            "model": "gpt-5.1", "user_template": "check {item}", "collection": ["a"], "endpoint": ep_json,
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test()).expect("mock answers");
        mock.assert_hits(1);
        let recs = as_values(&out.flow_records);
        let rec = crate::usage::assert_one_usage_record(&recs, crate::usage::CallKind::MapItem, "dispatch.map item (hosted)");
        let p = &rec["payload"];
        assert_eq!(p["requested_model"], "gpt-5.1");
        assert_eq!(p["reported_model"], "served-by-mock");
        let ep: darkmux_types::ModelEndpoint = serde_json::from_value(ep_json).unwrap();
        assert_eq!(p["endpoint"], crate::target::endpoint_route_label(&ep, "gpt-5.1"));
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL
    fn usage_conformance_map_item_without_usage_is_absent() {
        let server = httpmock::MockServer::start();
        let _mock = usage_mock(&server, false);
        let s = map_step(json!({ "model": "qwen3-4b", "user_template": "check {item}", "collection": ["a"] }));
        let out = with_lmstudio_url(&server.base_url(), || {
            DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new(), &crate::step_kinds::StepRunCtx::for_test())
        })
        .expect("mock answers");
        let recs = as_values(&out.flow_records);
        let rec = crate::usage::assert_one_usage_record(&recs, crate::usage::CallKind::MapItem, "dispatch.map item (no usage)");
        assert_eq!(rec["payload"]["token_source"], "absent");
    }
}
