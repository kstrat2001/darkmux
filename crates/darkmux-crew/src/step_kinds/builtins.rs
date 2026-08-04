//! Built-in step kinds (#1230 Packet 2): `dispatch.internal`,
//! `dispatch.single_shot`, `procedural.shell`, `procedural.noop`.
//!
//! Each kind reads its parameters from `Step.config` (a flat
//! `serde_json::Value` object — the same "kind-specific overflow bag"
//! pattern `WorkloadSpec.extras` and `ProfileModel.extras` already use).
//! Required keys are named in each kind's doc comment; a missing
//! required key is a loud `Err`, never a silent default that would mask
//! an operator/caller typo.
//!
//! **This is Tier 1 (#1352).** Every kind below is generic AND
//! config-driven — no per-mission control flow, only values read from
//! `Step.config`. This is the DEFAULT: before writing a new `StepKind`
//! anywhere (this crate's `step_kinds::patterns`, or bespoke inside a
//! mission's own module), check whether the actual need is just new
//! CONFIG on one of these four kinds. See `step_kinds::patterns`'s module
//! doc for the full three-tier picture.

use super::types::{
    MapDispatchOverride, MapRemoteBucket, OverrideDispatchCall, StepKind, StepOutcome, StepRunCtx,
};
use crate::types::{Step, Task};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// (#1442) The named per-item reason a `dispatch.map` hosted item records
/// when the remote per-execution bucket refuses its FIRST attempt. Public
/// (and `const`) because downstream reconstruction — the review pipeline's
/// dedup boundary rebuilding per-seat member accounting from
/// [`MapItemResult`]s — must distinguish a budget SKIP (call never fired;
/// not a draw) from a dispatch ERROR (call fired and failed; a real draw),
/// and matching this one canonical string is how it does so without the
/// generic block growing a domain-shaped result field.
pub const MAP_BUDGET_SKIP_ERROR: &str =
    "remote token budget exhausted for this step — call skipped";

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

fn config_str<'a>(step: &'a Step, key: &str) -> Option<&'a str> {
    step.config.get(key).and_then(|v| v.as_str())
}

fn require_config_str<'a>(step: &'a Step, kind_id: &str, key: &str) -> Result<&'a str> {
    config_str(step, key).ok_or_else(|| {
        anyhow!("step `{}`: `{kind_id}` requires config.{key}", step.id)
    })
}

/// (#1230 Packet 3) Best-effort role→profile→model resolution for
/// `StepKind::residency()` implementations — NOT the dispatch's own strict
/// preflight (that still runs in full, separately, inside `dispatch::
/// dispatch`/`dispatch_internal::dispatch` when the step actually
/// executes). This is purely a scheduling-classification hint: resolve
/// `role_id` against the named (or default) profile via the same
/// `select_model` scoring every dispatch preflight uses, and — if the
/// winning model is local (not endpoint-bearing) — return the `Placement`
/// gestalt's wave planner should reason about for it.
///
/// Fails open to `None` (→ `Residency::Remote`, today's behavior for every
/// kind) on ANY resolution hiccup: unresolvable role, unloadable registry,
/// no active profile, a remote-endpoint model, or a local model missing
/// `n_ctx`. A misclassification here costs a missed RAM-safety
/// optimization, never a hard failure — see the trait doc on `residency`.
///
/// **The fail-open is not free (#1509 review finding).** `Residency::Remote`
/// means `run_step_graph` never calls `ensure_wave_loaded` for this step —
/// no wave load, and (load-bearing) **no #1487 residency lease written**, so
/// the dispatch runs completely UNPROTECTED against a concurrent command's
/// `Exclusive` reconcile, which can evict its model mid-generation with zero
/// warning. That's the correct, silent behavior for a GENUINELY remote
/// (endpoint-bearing) model — it was never going to touch local residency.
/// It is a SILENT SAFETY GAP for every other fail-open cause (unresolvable
/// role, unloadable registry, no active profile, a local model missing
/// `n_ctx`) — those mean "this SHOULD have been a local, lease-protected
/// dispatch, but resolution broke," and voiding the #1487 protection with no
/// signal is exactly the "loud beats quiet" anti-pattern `CLAUDE.md` names.
/// [`resolve_local_placement_or_warn`] is that distinction made real: it
/// classifies WHICH fail-open reason fired and `eprintln!`s a named warning
/// for every cause except the legitimate remote-model one.
pub fn resolve_local_placement(
    role_id: &str,
    profile_name: Option<&str>,
    config_path: Option<&str>,
    seat: &str,
) -> Option<darkmux_gestalt::Placement> {
    resolve_local_placement_or_warn(role_id, profile_name, config_path, seat)
}

/// Why [`resolve_local_placement_inner`] returned no `Placement` — see that
/// function's doc. `Remote` is the legitimate, silent case (an
/// endpoint-bearing model was never going to need local residency);
/// `ResolutionFailed` is the loud one (see [`resolve_local_placement`]'s own
/// doc on why fail-open silence there is a real safety gap).
enum PlacementMiss {
    Remote,
    ResolutionFailed(String),
}

/// [`resolve_local_placement`]'s actual body, classified per [`PlacementMiss`]
/// instead of collapsing every miss to a bare `None`. `eprintln!`s a named
/// warning on every `ResolutionFailed` cause (never on the legitimate
/// `Remote` case) before falling open to `None`, same return contract as
/// before.
fn resolve_local_placement_or_warn(
    role_id: &str,
    profile_name: Option<&str>,
    config_path: Option<&str>,
    seat: &str,
) -> Option<darkmux_gestalt::Placement> {
    match resolve_local_placement_inner(role_id, profile_name, config_path, seat) {
        Ok(placement) => Some(placement),
        Err(PlacementMiss::Remote) => None,
        Err(PlacementMiss::ResolutionFailed(reason)) => {
            eprintln!(
                "darkmux: step `{seat}` (role `{role_id}`) could not resolve a LOCAL residency \
                 placement ({reason}) — classifying this dispatch Remote. If this role/profile IS \
                 meant to run a local model, this dispatch just lost the #1487 residency-lease \
                 protection: a concurrent darkmux command's Exclusive reconcile could evict its \
                 model mid-generation with no further warning. (This is a best-effort SCHEDULING \
                 classification, not the dispatch's own strict preflight — that still runs \
                 separately and may itself fail loud.)"
            );
            None
        }
    }
}

/// The classified resolution chain — see [`PlacementMiss`] and
/// [`resolve_local_placement_or_warn`]. Pure logic, no `eprintln!` here (the
/// caller owns deciding which `Err` variant is worth surfacing).
fn resolve_local_placement_inner(
    role_id: &str,
    profile_name: Option<&str>,
    config_path: Option<&str>,
    seat: &str,
) -> std::result::Result<darkmux_gestalt::Placement, PlacementMiss> {
    use crate::select::select_model;
    use PlacementMiss::ResolutionFailed;

    let loaded = darkmux_profiles::profiles::load_registry(config_path)
        .map_err(|e| ResolutionFailed(format!("profile registry: {e}")))?;
    let (_active_name, profile) = loaded
        .registry
        .resolve_active(profile_name)
        .ok_or_else(|| ResolutionFailed("no active profile".to_string()))?;

    let roles = crate::loader::load_roles().map_err(|e| ResolutionFailed(format!("loading roles: {e}")))?;
    let role = roles
        .iter()
        .find(|r| r.id == role_id)
        .ok_or_else(|| ResolutionFailed(format!("role `{role_id}` not found")))?;

    let skill_index: std::collections::HashMap<String, crate::types::Skill> =
        crate::loader::load_skills()
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect();
    let model_id = select_model(role, profile, |id| skill_index.get(id))
        .map_err(|e| ResolutionFailed(format!("select_model: {e}")))?;
    let pm = profile
        .models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| ResolutionFailed(format!("selected model `{model_id}` not found in profile")))?;
    if pm.is_remote() {
        return Err(PlacementMiss::Remote);
    }
    let min_ctx = pm
        .n_ctx
        .ok_or_else(|| ResolutionFailed(format!("model `{}` has no declared n_ctx", pm.id)))?;
    let identifier = darkmux_gestalt::namespaced_identifier(&pm.id, pm.identifier.as_deref());
    Ok(darkmux_gestalt::Placement {
        model_key: pm.id.clone(),
        identifier,
        min_ctx,
        seat: seat.to_string(),
    })
}

/// (#1230 Packet 4 DRY pass) One `failed_tool_invocations` entry from the
/// internal runtime's `--json` envelope — a verifier command the dispatched
/// role's tool loop attempted to run but never actually executed (missing
/// binary, toolchain not present, etc). Moved here from `src/coder_phase.rs`
/// (was mission-run-private) so ANY `dispatch.internal`-shaped step can
/// surface it, not just `mission.coder` — see `parse_failed_verifiers` and
/// `DispatchInternalStepKind`'s `parse_verifiers` config opt-in below.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FailedVerifier {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub reason: String,
}

/// Best-effort parse of `failed_tool_invocations` from the internal
/// runtime's `--json` envelope (a dispatch's stdout). In `--json` mode the
/// runtime prints a single-line JSON envelope to stdout (status goes to
/// stderr), so the whole buffer is the envelope; the last-non-empty-line
/// fallback is pure defense against an unexpected leading line. Returns
/// EMPTY on any parse miss or absent field — a soft signal must never fire
/// a FALSE alarm, so "couldn't tell" reads as "nothing failed."
pub fn parse_failed_verifiers(envelope_stdout: &str) -> Vec<FailedVerifier> {
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
                .filter_map(|e| serde_json::from_value::<FailedVerifier>(e.clone()).ok())
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
/// `config_str` reads `None` and the original behavior is byte-identical.
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
/// `FailedVerifier` above.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RawDispatchOutcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_dir: Option<std::path::PathBuf>,
}

/// `task.<field>.clone()`, falling back to `Step.config.<key>` (as a
/// string) when the Task leaves it unset — the shared sourcing rule every
/// dispatch-shaped built-in's assignment fields use (#1230/#1341).
fn task_or_config_str(task_field: Option<&String>, step: &Step, key: &str) -> Option<String> {
    task_field.cloned().or_else(|| config_str(step, key).map(str::to_string))
}

impl StepKind for DispatchInternalStepKind {
    fn id(&self) -> &'static str {
        "dispatch.internal"
    }

    fn display_name(&self) -> &'static str {
        "Dispatch"
    }

    fn run(&self, step: &Step, task: &Task, input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        use crate::dispatch::{dispatch, CompactionDispatchArgs, DispatchOpts};

        let role_id = task_or_config_str(task.role_id.as_ref(), step, "role_id").ok_or_else(|| {
            anyhow!("step `{}`: `{}` requires task.role_id or config.role_id", step.id, self.id())
        })?;
        let base_message = config_str(step, "message").unwrap_or_default();
        let message = compose_message(base_message, input);
        let timeout_seconds = step
            .config
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600) as u32;
        let profile_name = task_or_config_str(task.profile_name.as_ref(), step, "profile_name");
        let image = task_or_config_str(task.image.as_ref(), step, "image");
        let config_path = config_str(step, "config_path").map(str::to_string);
        let workdir = task
            .workdir
            .clone()
            .or_else(|| config_str(step, "workdir").map(std::path::PathBuf::from));
        let phase_id = config_str(step, "phase_id").map(str::to_string);
        let session_id = config_str(step, "session_id")
            .map(str::to_string)
            .unwrap_or_else(|| darkmux_types::session_id::step(&step.id));
        let parse_verifiers = step
            .config
            .get("parse_verifiers")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // (#1509) Additive, default-preserving config passthroughs — see
        // `DispatchInternalStepKind`'s doc. Every existing caller (mission
        // launch, coder-phase, review) never sets these keys, so each falls
        // back to the exact literal the code used to hardcode here.
        let skip_preflight = step
            .config
            .get("skip_preflight")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let json = step.config.get("json").and_then(|v| v.as_bool()).unwrap_or(true);
        let max_completion_tokens = step
            .config
            .get("max_completion_tokens")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());
        let preserve_dispatch_result = step
            .config
            .get("preserve_dispatch_result")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let opts = DispatchOpts {
            role_id,
            message,
            session_id: Some(session_id),
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
            // records attribute to this seat even if `session_id` was
            // config-overridden off the `step-<id>` default the viewer maps.
            step_id: Some(step.id.clone()),
        };
        let result =
            dispatch(opts).with_context(|| format!("step `{}` dispatch.internal", step.id))?;

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
                out_dir: result.out_dir,
            };
            let output = serde_json::to_string(&payload)
                .context("serializing RawDispatchOutcome for preserve_dispatch_result")?;
            return Ok(StepOutcome { output, flow_records: Vec::new() });
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
                    ts: darkmux_flow::ts_utc_now(),
                    level: darkmux_flow::Level::Warn,
                    category: darkmux_flow::Category::Work,
                    tier: darkmux_flow::Tier::Local,
                    stage: darkmux_flow::Stage::Dispatch,
                    action: "step result".to_string(),
                    handle: step.id.clone(),
                    phase_id: None,
                    session_id: Some(darkmux_types::session_id::task(&step.task_id)),
                    source: Some("scheduler".to_string()),
                    model: None,
                    reasoning: None,
                    mission_id: None,
                    machine_id: None,
                    machine_uid: None,
                    orchestrator: None,
                    prev_hash: None,
                    hash: None,
                    payload: Some(serde_json::json!({
                        "step_id": step.id,
                        "kind": "dispatch.internal",
                        "failed_verifiers": failed,
                        "count": failed.len(),
                    })),
                    work_id: None,
                    attempt: None,
                });
            }
        }

        Ok(StepOutcome {
            output: result.stdout,
            flow_records,
        })
    }

    fn residency(
        &self,
        step: &Step,
        task: &Task,
        _input: &std::collections::BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> Option<darkmux_gestalt::Placement> {
        let role_id = task_or_config_str(task.role_id.as_ref(), step, "role_id")?;
        let profile_name = task_or_config_str(task.profile_name.as_ref(), step, "profile_name");
        let config_path = config_str(step, "config_path").map(str::to_string);
        // NOTE: `step:{id}` here is a gestalt SEAT LABEL (placement-plan
        // diagnostics), NOT a flow-record session id — exempt from the #1436
        // hyphen convention; future colon sweeps should skip it.
        resolve_local_placement(&role_id, profile_name.as_deref(), config_path.as_deref(), &format!("step:{}", step.id))
    }
}

/// (#1412) Clamp a requested `max_tokens` down to the per-execution remote
/// token allowance so one hosted call cannot request more completion
/// tokens than the whole execution is allowed to spend. `budget == 0` is
/// unreachable in practice (the hosted arm calls `admit_remote_execution`
/// first, which already refuses a zero budget), but the clamp stays
/// total/defensive rather than assuming its caller's ordering. A `budget`
/// wider than `u32::MAX` (the allowance is `u64`, `max_tokens` on the wire
/// is `u32`) saturates instead of wrapping.
fn clamp_hosted_max_tokens(requested: u32, budget: u64) -> u32 {
    let budget_u32 = u32::try_from(budget).unwrap_or(u32::MAX);
    requested.min(budget_u32)
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
/// **Hosted-arm metering (#1412).** The LOCAL dialect (LMStudio) is
/// unmetered by design — `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION` governs
/// REMOTE spend only. The HOSTED dialect now goes through the same minimum
/// gate `dispatch_remote` uses (`dispatch_internal::admit_remote_execution`)
/// before the call fires, so a `0` allowance refuses with a typed error
/// instead of dispatching off the meter, plus a `max_tokens` clamp
/// (`clamp_hosted_max_tokens`) so one call can't request more than the
/// allowance in one shot. **Reading:** each `dispatch.single_shot` step is
/// its own execution (one pipeline stage) — a graph with several
/// endpoint-bearing single-shot steps draws the allowance once PER STEP,
/// not once for the whole graph, matching how a bare `dispatch` (also
/// gated by `admit_remote_execution`) counts as one execution. This is the
/// minimum regime, not the full one: there is no cross-call bucket here
/// (each step gets a fresh allowance check), unlike the review funnel's
/// per-stage `RemoteBucket` (`darkmux-lab::lab::review`), which accumulates
/// spend across many calls in one stage. `darkmux-lab` depends on
/// `darkmux-crew`, not the reverse, so `RemoteBucket` cannot be reused here
/// without moving it — that consolidation is #1414's job. This PR closes
/// the silent-bypass gap (#1412); the shared-bucket regime is a deliberate
/// follow-up, not a scope cut hiding in this diff.
pub struct DispatchSingleShotStepKind;

impl StepKind for DispatchSingleShotStepKind {
    fn id(&self) -> &'static str {
        "dispatch.single_shot"
    }

    fn display_name(&self) -> &'static str {
        "Dispatch (single-shot)"
    }

    fn run(&self, step: &Step, _task: &Task, input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        use crate::single_shot::{
            single_shot_chat, single_shot_chat_hosted, HostedSingleShotRequest,
            SingleShotRequest,
        };

        let model = require_config_str(step, self.id(), "model")?;
        let system = config_str(step, "system").unwrap_or("");
        let base_user = config_str(step, "user").unwrap_or_default();
        let user = compose_message(base_user, input);
        let max_tokens = step
            .config
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(4096) as u32;
        let timeout_seconds = step
            .config
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(120) as u32;

        let mut flow_records = Vec::new();

        let reply = if let Some(endpoint_val) = step.config.get("endpoint") {
            let endpoint: darkmux_types::ModelEndpoint = serde_json::from_value(endpoint_val.clone())
                .with_context(|| format!("step `{}`: config.endpoint", step.id))?;

            // (#1412) Admit gate FIRST — a budget of 0 refuses before any
            // HTTP call is even constructed, mirroring `dispatch_remote`'s
            // ordering (meter before the network, never after). No
            // `.with_context` wrap here on purpose: `admit_remote_execution`
            // already names the step-independent bucket reason in full, the
            // same bare error `dispatch_remote` surfaces — wrapping it would
            // just bury that message under a second "step `s1` ..." layer.
            let budget = darkmux_types::config_access::remote_max_tokens_per_execution();
            crate::dispatch_internal::admit_remote_execution(budget)?;

            let clamped_max_tokens = clamp_hosted_max_tokens(max_tokens, budget);
            let req = HostedSingleShotRequest {
                endpoint: &endpoint,
                model,
                system,
                user: &user,
                max_tokens: clamped_max_tokens,
                timeout_seconds,
            };
            let reply = single_shot_chat_hosted(&req)
                .with_context(|| format!("step `{}` dispatch.single_shot (hosted)", step.id))?;

            // (#1412) Surface actual spend the same way `dispatch_remote`
            // embeds totals in its `dispatch complete` record, so a hosted
            // single-shot step's token usage is visible even without the
            // full per-stage bucket regime.
            flow_records.push(darkmux_flow::FlowRecord {
                ts: darkmux_flow::ts_utc_now(),
                level: darkmux_flow::Level::Info,
                category: darkmux_flow::Category::Work,
                tier: darkmux_flow::Tier::Local,
                stage: darkmux_flow::Stage::Dispatch,
                action: "step result".to_string(),
                handle: step.id.clone(),
                phase_id: None,
                session_id: Some(darkmux_types::session_id::task(&step.task_id)),
                source: Some("scheduler".to_string()),
                model: Some(model.to_string()),
                reasoning: None,
                mission_id: None,
                machine_id: None,
                machine_uid: None,
                orchestrator: None,
                prev_hash: None,
                hash: None,
                payload: Some(serde_json::json!({
                    "step_id": step.id,
                    "kind": "dispatch.single_shot",
                    "runtime": "direct",
                    "remote_max_tokens_per_execution": budget,
                    "max_tokens_requested": max_tokens,
                    "max_tokens_sent": clamped_max_tokens,
                    "prompt_tokens": reply.prompt_tokens,
                    "completion_tokens": reply.completion_tokens,
                    "total_tokens": reply.total_tokens,
                })),
                work_id: None,
                attempt: None,
            });

            reply
        } else {
            let temperature = step
                .config
                .get("temperature")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.7) as f32;
            let req = SingleShotRequest {
                base_url: None,
                model,
                system,
                user: &user,
                temperature,
                max_tokens,
                timeout_seconds,
            };
            single_shot_chat(&req)
                .with_context(|| format!("step `{}` dispatch.single_shot (local)", step.id))?
        };

        Ok(StepOutcome {
            output: reply.content,
            flow_records,
        })
    }
}

// ─── dispatch.map (#1442) ───────────────────────────────────────────────

// (#1442) `MapRemoteBucket` moved to `super::types` so the SCHEDULER can
// own a `bucket_group -> Arc<Mutex<MapRemoteBucket>>` map and hand the same
// bucket to sibling `dispatch.map` steps (the "allowance multiplication"
// carry-forward — see that type's doc and `StepRunCtx`). The budget-0
// divergence (a grouped-or-ungrouped `dispatch.map` completes `Ok` with
// every item skipped rather than a step-level `Err`, unlike
// `dispatch.single_shot`'s hosted arm) is unchanged and documented on
// `run` below.

/// (#1442 gate C4) What one hosted map item SPENDS from the bucket: the
/// reply's reported `usage.total_tokens` when present, else — conservatively
/// — the clamped `max_tokens` the call was granted. An endpoint that omits
/// usage entirely must not mint an infinite allowance (spending 0 per call
/// would let an omitting endpoint dispatch the whole collection off the
/// meter); over-counting a capped grant is the safe direction.
fn conservative_hosted_spend(total_tokens: Option<u64>, granted_max_tokens: u32) -> u64 {
    total_tokens.unwrap_or(u64::from(granted_max_tokens))
}

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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

/// Resolve a `dispatch.map` step's collection. Precedence: an explicit
/// `config.collection` JSON array wins; otherwise the collection is a RUNTIME
/// INPUT — the dependency output named by `config.collection_input`, or (when
/// the step has exactly one dependency) that one input.
///
/// **Loud on every typo-shaped source (#1442 gate — the #1418 class: a
/// silent empty masks a config typo as a clean no-op run):**
/// - a present-but-non-array `config.collection` is a loud `Err`, never a
///   silent fall-through to input resolution;
/// - a `collection_input` naming a key ABSENT from `input` is a loud `Err`
///   naming the missing key AND the inputs actually present;
/// - two or more dependency inputs with NO `collection_input` is a loud
///   `Err` ("name one") — returning empty there would not be a refusal to
///   guess, it would BE a guess ("there is no collection");
/// - a present-but-non-array INPUT source is a loud `Err` too.
///
/// What stays a REAL empty (and short-circuits cleanly): a genuinely absent
/// source (no `collection` key, no `collection_input`, zero dependency
/// inputs) and a present-but-blank source string (the upstream truly
/// produced nothing). `run` surfaces every `Err` here; `residency` stays
/// best-effort and swallows them (see its doc).
fn resolve_map_collection(
    step: &Step,
    input: &BTreeMap<String, String>,
) -> Result<Vec<serde_json::Value>> {
    if let Some(v) = step.config.get("collection") {
        return match v.as_array() {
            Some(arr) => Ok(arr.clone()),
            None => bail!(
                "step `{}`: `dispatch.map` config.collection must be a JSON array",
                step.id
            ),
        };
    }
    let present_keys = || {
        if input.is_empty() {
            "none".to_string()
        } else {
            input.keys().cloned().collect::<Vec<_>>().join(", ")
        }
    };
    let source: Option<&String> = match config_str(step, "collection_input") {
        Some(key) => match input.get(key) {
            Some(s) => Some(s),
            None => bail!(
                "step `{}`: `dispatch.map` config.collection_input names `{key}`, which is \
                 not among this step's dependency inputs (present: {})",
                step.id,
                present_keys()
            ),
        },
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
/// per-item record shape — with a per-item loop and a step-scoped remote
/// bucket ([`MapRemoteBucket`]) added on top. That there is no genuinely-new
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
/// **`retry_on_empty` (#1442, the generic port of the probe stage's
/// retry-on-empty loop).** Default `0` (off) — a call whose trimmed content
/// comes back empty is accepted as-is (`ok: true`, empty `content`). When set
/// to `N > 0`, an empty-content reply is RE-DISPATCHED up to `N` additional
/// times (so `N = 1` matches the review probe's historical single retry: up
/// to 2 attempts total), stopping early the moment a non-empty reply lands.
/// Tokens are accumulated across EVERY attempt (an empty reasoning-model reply
/// still burns — and is billed — its whole completion budget), and the hosted
/// arm draws from the remote bucket on each attempt (a retry is another
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
    /// One per-item flow record, field-aligned with
    /// [`DispatchSingleShotStepKind`]'s hosted "step result" record so a
    /// graph/parity consumer reads a map's per-item records the same way it
    /// reads a single-shot's.
    fn item_record(step: &Step, model: &str, remote: bool, res: &MapItemResult) -> darkmux_flow::FlowRecord {
        darkmux_flow::FlowRecord {
            ts: darkmux_flow::ts_utc_now(),
            level: if res.ok { darkmux_flow::Level::Info } else { darkmux_flow::Level::Warn },
            category: darkmux_flow::Category::Work,
            tier: darkmux_flow::Tier::Local,
            stage: darkmux_flow::Stage::Dispatch,
            action: "step result".to_string(),
            handle: step.id.clone(),
            phase_id: None,
            session_id: Some(darkmux_types::session_id::task(&step.task_id)),
            source: Some("scheduler".to_string()),
            model: Some(model.to_string()),
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: Some(serde_json::json!({
                "step_id": step.id,
                "kind": "dispatch.map",
                "index": res.index,
                "ok": res.ok,
                "remote": remote,
                "total_tokens": res.total_tokens,
                // (#1442) Per-item telemetry: the endpoint-reported served
                // model (HOSTED only; `None` for a local item, by
                // construction) and this item's cumulative dispatch wall-clock
                // across every attempt.
                "served_model": res.served_model,
                "wall_ms": res.wall_ms,
                "error": res.error,
            })),
            work_id: None,
            attempt: None,
        }
    }

    /// (#1442 gate C1) The ONE step-level aggregate record emitted after the
    /// whole loop: items_in, ok_count, failed_count, remote, and SUMMED
    /// total_tokens across every item. See the emission site in `run` for
    /// why the sum (not the per-item values) is what the mission graph's
    /// max-fold token meter must see.
    /// (#1607) The dispatch-liveness bookends this kind owes contract #2:
    /// "any production code path that performs model work emits
    /// `dispatch.start` and a terminal `dispatch.complete`/`dispatch.error`
    /// ... new vocabularies supplement, never replace."
    ///
    /// `dispatch.map` emitted only its own `step result` vocabulary, so the
    /// per-seat sessions it mints (`task-<id>` — the review's probe and verify
    /// seats) had token records but nothing anywhere naming WHERE they ran.
    /// The savings hero reads `payload.endpoint` off these bookends and off
    /// nothing else, so hosted seat spend was unattributable: 229,034 tokens
    /// on one machine in one day.
    ///
    /// `endpoint_label` is `None` for a local seat, which leaves the payload
    /// byte-identical to a purely-local dispatch's — the same no-op-when-None
    /// discipline `stamp_remote_classification` keeps.
    fn bookend_record(
        step: &Step,
        model: &str,
        action: &str,
        level: darkmux_flow::Level,
        endpoint_label: Option<&str>,
        extra: serde_json::Value,
    ) -> darkmux_flow::FlowRecord {
        let mut payload = serde_json::json!({
            "step_id": step.id,
            "kind": "dispatch.map",
            "runtime": "scheduler",
        });
        if let (Some(obj), Some(ex)) = (payload.as_object_mut(), extra.as_object()) {
            for (k, v) in ex {
                obj.insert(k.clone(), v.clone());
            }
        }
        darkmux_flow::stamp_remote_classification(&mut payload, endpoint_label, None);
        darkmux_flow::FlowRecord {
            ts: darkmux_flow::ts_utc_now(),
            level,
            category: darkmux_flow::Category::Work,
            tier: darkmux_flow::Tier::Local,
            stage: darkmux_flow::Stage::Dispatch,
            action: action.to_string(),
            handle: step.id.clone(),
            phase_id: None,
            // The SAME session id the item/aggregate records use — that is
            // what lets a consumer join a seat's tokens to its endpoint.
            session_id: Some(darkmux_types::session_id::task(&step.task_id)),
            source: Some("scheduler".to_string()),
            model: Some(model.to_string()),
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

    fn aggregate_record(
        step: &Step,
        model: &str,
        remote: bool,
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
            ts: darkmux_flow::ts_utc_now(),
            level: if failed_count == 0 { darkmux_flow::Level::Info } else { darkmux_flow::Level::Warn },
            category: darkmux_flow::Category::Work,
            tier: darkmux_flow::Tier::Local,
            stage: darkmux_flow::Stage::Dispatch,
            action: "step result".to_string(),
            handle: step.id.clone(),
            phase_id: None,
            session_id: Some(darkmux_types::session_id::task(&step.task_id)),
            source: Some("scheduler".to_string()),
            model: Some(model.to_string()),
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: Some(serde_json::json!({
                "step_id": step.id,
                "kind": "dispatch.map",
                "items_in": results.len(),
                "ok_count": ok_count,
                "failed_count": failed_count,
                "remote": remote,
                "total_tokens": total_tokens,
                "total_wall_ms": total_wall_ms,
            })),
            work_id: None,
            attempt: None,
        }
    }

    /// The empty-collection short-circuit record (#1442): a NAMED reason so
    /// observability answers "why did this map not dispatch" directly.
    fn short_circuit_record(step: &Step) -> darkmux_flow::FlowRecord {
        darkmux_flow::FlowRecord {
            ts: darkmux_flow::ts_utc_now(),
            level: darkmux_flow::Level::Info,
            category: darkmux_flow::Category::Work,
            tier: darkmux_flow::Tier::Local,
            stage: darkmux_flow::Stage::Dispatch,
            action: "step result".to_string(),
            handle: step.id.clone(),
            phase_id: None,
            session_id: Some(darkmux_types::session_id::task(&step.task_id)),
            source: Some("scheduler".to_string()),
            model: config_str(step, "model").map(str::to_string),
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: Some(serde_json::json!({
                "step_id": step.id,
                "kind": "dispatch.map",
                "items_in": 0,
                "items_out": 0,
                "short_circuit": "empty collection — dispatch.map skipped before any model load",
            })),
            work_id: None,
            attempt: None,
        }
    }

    /// (#1605) Shared parse/validate for a `dispatch.map` retry-budget
    /// config key (`retry_on_empty`, `retry_on_error`) — both are a
    /// non-negative integer, default 0/off when absent, and a loud
    /// step-run-time `Err` (never silent coercion) when present but
    /// non-integer or out of `u32`'s range. Named after the key it reads so
    /// the error messages stay specific to whichever knob was misconfigured.
    fn config_retry_budget(&self, step: &Step, key: &'static str) -> Result<u32> {
        match step.config.get(key) {
            None => Ok(0),
            Some(v) => {
                let n = v.as_u64().ok_or_else(|| {
                    anyhow!("step `{}`: `{}` config.{key} must be a non-negative integer", step.id, self.id())
                })?;
                u32::try_from(n).map_err(|_| {
                    anyhow!(
                        "step `{}`: `{}` config.{key} ({n}) exceeds the maximum of {}",
                        step.id,
                        self.id(),
                        u32::MAX
                    )
                })
            }
        }
    }

    /// (#1442) The shared map body behind both the ctx-free [`StepKind::run`]
    /// and the streaming [`StepKind::run_streaming`]. `ctx` is `None` for the
    /// unit-test/no-scheduler path (records batch into
    /// `StepOutcome.flow_records`, bucket is step-scoped) and `Some` for the
    /// scheduler path (records emit LIVE, a named `bucket_group` shares one
    /// allowance across sibling steps).
    fn run_map(
        &self,
        step: &Step,
        input: &BTreeMap<String, String>,
        ctx: Option<&StepRunCtx>,
    ) -> Result<StepOutcome> {
        let items = resolve_map_collection(step, input)?;
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
            push(Self::short_circuit_record(step), &mut batched);
            return Ok(StepOutcome { output: "[]".to_string(), flow_records: batched });
        }

        let model = require_config_str(step, self.id(), "model")?;
        let user_template = require_config_str(step, self.id(), "user_template")?;
        let system = config_str(step, "system").unwrap_or("");
        let max_tokens = step.config.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(4096) as u32;
        let timeout_seconds =
            step.config.get("timeout_seconds").and_then(|v| v.as_u64()).unwrap_or(120) as u32;
        // (#1442) The generic retry-on-empty budget (default 0/off) — see the
        // struct doc. Read once for the whole collection loop. ABSENT → 0
        // (optional, off). A PRESENT-but-invalid value is a LOUD config error
        // at step-run time (matching this block's `require_config_*`
        // "missing/invalid key is loud" doctrine), never silently coerced —
        // an out-of-u32-range `retry_on_empty` must NOT become ~4 billion
        // re-dispatches (the prior `u32::try_from(...).unwrap_or(u32::MAX)`
        // did exactly that; #1442 gate CONSIDER).
        let retry_on_empty = self.config_retry_budget(step, "retry_on_empty")?;
        // (#1605) `retry_on_error` — same shape, same validation, default
        // 0/off. See [`DispatchMapStepKind`]'s doc for the policy this opts
        // a step INTO: a dispatch `Err` is retried up to this many times
        // (short backoff between attempts) instead of isolating immediately.
        // Off by default for every existing caller; the review pipeline's
        // probe stage is the first to set it (darkmux#1605 cause 2).
        let retry_on_error = self.config_retry_budget(step, "retry_on_error")?;
        let endpoint: Option<darkmux_types::ModelEndpoint> = match step.config.get("endpoint") {
            Some(v) => Some(
                serde_json::from_value(v.clone())
                    .with_context(|| format!("step `{}`: config.endpoint", step.id))?,
            ),
            None => None,
        };

        // (#1607) Contract #2's liveness bookends. Opened BEFORE any model
        // work and closed on every exit path, including the ones a `?` takes:
        // `MapBookend`'s Drop emits `dispatch error` unless `close` already
        // consumed the terminal. This is what gives a per-seat `task-<id>`
        // session an endpoint to be attributed by — without it the seat's
        // token records name a model and a cost but never a place.
        let endpoint_label: Option<String> = endpoint
            .as_ref()
            .map(|ep| crate::dispatch_internal::remote_endpoint_label(ep, model));
        let mut bookend = MapBookend::new(
            ctx,
            Self::bookend_record(
                step,
                model,
                "dispatch start",
                darkmux_flow::Level::Info,
                endpoint_label.as_deref(),
                serde_json::json!({ "items_in": items.len() }),
            ),
            Self::bookend_record(
                step,
                model,
                "dispatch error",
                darkmux_flow::Level::Error,
                endpoint_label.as_deref(),
                serde_json::json!({
                    "result_class": "error",
                    "error": "dispatch.map terminated before completion (early return or panic)",
                }),
            ),
        );

        // Per-EXECUTION remote allowance. When the step named a
        // `bucket_group`, the SCHEDULER already resolved the group's SHARED
        // bucket and handed it in through `ctx.remote_bucket()` — every
        // sibling step of the group meters one allowance BETWEEN them
        // (#1442, the allowance-multiplication fix). Ungrouped (or ctx-free)
        // steps get their own step-scoped bucket from the same budget, so
        // the one-execution contract reads identically either way. Local
        // items never draw from it. `bucket_budget` (u64, optional) lets a
        // LAUNCHER stamp its already-resolved per-execution allowance into
        // the step's own config — self-describing config, and the same
        // value the scheduler honors when it creates a group bucket —
        // instead of this block re-reading the environment at run time;
        // absent, the `config_access` resolution applies as before.
        let bucket: Arc<Mutex<MapRemoteBucket>> = match ctx.and_then(|c| c.remote_bucket()) {
            Some(shared) => shared.clone(),
            None => {
                let budget = step
                    .config
                    .get("bucket_budget")
                    .and_then(|v| v.as_u64())
                    .unwrap_or_else(darkmux_types::config_access::remote_max_tokens_per_execution);
                Arc::new(Mutex::new(MapRemoteBucket::new(budget)))
            }
        };
        // (#1442 ship-2b) The scheduler-supplied dispatch override, if any —
        // threaded into every item's arm; `None` on all production paths.
        let ovr = ctx.and_then(|c| c.dispatch_override());

        let temperature =
            step.config.get("temperature").and_then(|v| v.as_f64()).unwrap_or(0.7) as f32;
        let mut results: Vec<MapItemResult> = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let user = user_template.replace("{item}", &map_item_text(item));
            let res = match &endpoint {
                Some(ep) => map_hosted_item(
                    index, &bucket, ep, model, system, &user, max_tokens, timeout_seconds,
                    retry_on_empty, retry_on_error, ovr,
                ),
                None => map_local_item(
                    index, model, system, &user, temperature, max_tokens, timeout_seconds,
                    retry_on_empty, retry_on_error, ovr,
                ),
            };
            // (#1442 gate C3) LIVE per-item emission when streaming.
            push(Self::item_record(step, model, endpoint.is_some(), &res), &mut batched);
            // (#1442 ship-2b, #1361 continuity) One `telemetry.tokens`
            // record per item that actually reported usage, so the fleet
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
            if let Some(payload) = map_item_token_payload(&res) {
                push(
                    crate::dispatch::build_telemetry_record(
                        darkmux_flow::Level::Info,
                        "telemetry.tokens",
                        "tokens",
                        &step.id,
                        &darkmux_types::session_id::task(&step.task_id),
                        Some(model),
                        None,
                        None,
                        payload,
                    ),
                    &mut batched,
                );
            }
            results.push(res);
        }

        // (#1442 gate C1) ONE step-level aggregate record after the loop.
        // Load-bearing: the mission graph's token meter folds "step result"
        // token fields per step with Math.max (a one-record-per-step
        // assumption) — with only N per-item records, a map step's meter
        // would render as the LARGEST single item, not the step's spend. The
        // aggregate's SUMMED total_tokens is >= every per-item value, so the
        // existing max-fold reads the true spend with zero viewer changes.
        push(Self::aggregate_record(step, model, endpoint.is_some(), &results), &mut batched);

        // (#1607) The clean terminal. `remote_tokens` is stamped only for a
        // hosted seat, and only the SUM the seat actually spent — the same
        // figure the aggregate reports, so the two never disagree.
        let ok_count = results.iter().filter(|r| r.ok).count();
        let spent: u64 = results.iter().filter_map(|r| r.total_tokens).sum();
        let mut done = Self::bookend_record(
            step,
            model,
            "dispatch complete",
            if ok_count == results.len() { darkmux_flow::Level::Info } else { darkmux_flow::Level::Warn },
            endpoint_label.as_deref(),
            serde_json::json!({
                "result_class": if ok_count == results.len() { "ok" } else { "partial" },
                "items_in": results.len(),
                "ok_count": ok_count,
                "failed_count": results.len() - ok_count,
            }),
        );
        if endpoint_label.is_some() {
            if let Some(payload) = done.payload.as_mut() {
                darkmux_flow::stamp_remote_classification(payload, None, Some(spent));
            }
        }
        bookend.close(done);

        let output = serde_json::to_string(&results).context("serializing dispatch.map results")?;
        Ok(StepOutcome { output, flow_records: batched })
    }
}

/// (#1607) RAII half of `dispatch.map`'s liveness bookends.
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
struct MapBookend<'a> {
    ctx: Option<&'a StepRunCtx>,
    /// The terminal to emit if dropped without `close` — taken by `close`.
    on_abort: Option<darkmux_flow::FlowRecord>,
}

impl<'a> MapBookend<'a> {
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
}

impl Drop for MapBookend<'_> {
    fn drop(&mut self) {
        if let (Some(rec), Some(c)) = (self.on_abort.take(), self.ctx) {
            c.emit(rec);
        }
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

/// (#1442) `total_tokens` for a per-item result: the accumulated sum across
/// every attempt, but only when at least one attempt actually reported usage.
/// A run where no attempt sent `usage` stays honest `None` — never a
/// fabricated `0` a run-level token sum would silently swallow (the same
/// discipline `conservative_hosted_spend`/the aggregate record already keep).
fn item_total_tokens(any_usage: bool, sum: u64) -> Option<u64> {
    any_usage.then_some(sum)
}

/// (#1530 dogfood) The `prompt_tokens`/`completion_tokens` sibling of
/// [`item_total_tokens`], with the SAME honesty rule: accumulate across every
/// attempt, but report `None` unless at least one attempt actually carried a
/// split. A provider that returns only `usage.total_tokens` therefore leaves
/// both fields absent rather than reporting a fabricated `0` — which the
/// dashboard would read as "this dispatch generated nothing", a worse lie
/// than "unknown".
///
/// Tracked separately from `any_usage` on purpose: a reply can carry a total
/// without a split, so the two flags are genuinely independent.
fn item_split_tokens(any_split: bool, sum: u64) -> Option<u64> {
    any_split.then_some(sum)
}

/// (#1530 dogfood) Pure: one map item's `telemetry.tokens` payload, or
/// `None` when the item reported no usage at all (no record is emitted then
/// — pre-existing behavior). Split out from the emitter so the payload SHAPE
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
fn map_item_token_payload(res: &MapItemResult) -> Option<serde_json::Value> {
    let total = res.total_tokens.or_else(|| match (res.prompt_tokens, res.completion_tokens) {
        (None, None) => None,
        (p, c) => Some(p.unwrap_or(0) + c.unwrap_or(0)),
    })?;
    let mut payload = serde_json::json!({ "total_tokens": total });
    let obj = payload.as_object_mut().expect("json! built an object");
    if let Some(p) = res.prompt_tokens {
        obj.insert("prompt_tokens".into(), serde_json::json!(p));
    }
    if let Some(c) = res.completion_tokens {
        obj.insert("completion_tokens".into(), serde_json::json!(c));
    }
    Some(payload)
}

/// (#1530 dogfood) Fold one reply's usage split into the running per-item
/// accumulators. Shared by the local and hosted arms so both report
/// identically — the token-accounting parity [`map_local_item`] and
/// [`map_hosted_item`] already keep for the total.
///
/// ASSUMPTION worth naming: a provider is expected to report usage
/// CONSISTENTLY across the attempts of one item. If attempt 1 returns a
/// total with no split and attempt 2 returns both, the item's accumulated
/// `total` covers both attempts while its split covers only the second, so
/// the dashboard's `total == prompt + completion` identity drifts for that
/// item. Nothing renders wrong (the unclassified bucket only ever adds), and
/// no provider we dispatch to behaves this way — recorded so a future reader
/// who hits it knows it was considered rather than missed.
fn accumulate_split(
    reply_prompt: Option<u64>,
    reply_completion: Option<u64>,
    psum: &mut u64,
    csum: &mut u64,
    any_split: &mut bool,
) {
    if reply_prompt.is_none() && reply_completion.is_none() {
        return;
    }
    *psum += reply_prompt.unwrap_or(0);
    *csum += reply_completion.unwrap_or(0);
    *any_split = true;
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
) -> MapItemResult {
    use crate::single_shot::{single_shot_chat, SingleShotRequest};
    let mut sum = 0u64;
    let mut any_usage = false;
    // (#1530 dogfood) Parallel to `sum`/`any_usage`, for the usage SPLIT.
    let (mut psum, mut csum, mut any_split) = (0u64, 0u64, false);
    // (#1442) Cumulative dispatch wall-clock across every attempt — the same
    // per-attempt accumulation `sum` (tokens) uses. A LOCAL item's
    // `served_model` is ALWAYS `None` by construction (see [`MapItemResult`]'s
    // doc): the response body's echoed `model` is not ground truth for a local
    // dispatch, so this arm never reads it.
    let mut wall_ms = 0u64;
    let mut empty_budget = retry_on_empty;
    let mut error_budget = retry_on_error;
    let mut error_retries_used = 0u32;
    loop {
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
        match dispatch {
            Ok(reply) => {
                if let Some(t) = reply.total_tokens {
                    sum += t;
                    any_usage = true;
                }
                accumulate_split(
                    reply.prompt_tokens,
                    reply.completion_tokens,
                    &mut psum,
                    &mut csum,
                    &mut any_split,
                );
                if !reply.content.trim().is_empty() {
                    return MapItemResult {
                        index,
                        ok: true,
                        content: reply.content,
                        error: None,
                        total_tokens: item_total_tokens(any_usage, sum),
                        prompt_tokens: item_split_tokens(any_split, psum),
                        completion_tokens: item_split_tokens(any_split, csum),
                        served_model: None,
                        wall_ms,
                        retried: error_retries_used,
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
                    return MapItemResult {
                        index,
                        ok: false,
                        content: String::new(),
                        error: Some(format!("{e:#}")),
                        total_tokens: item_total_tokens(any_usage, sum),
                        prompt_tokens: item_split_tokens(any_split, psum),
                        completion_tokens: item_split_tokens(any_split, csum),
                        served_model: None,
                        wall_ms,
                        retried: error_retries_used,
                    };
                }
                error_budget -= 1;
                error_retries_used += 1;
                std::thread::sleep(RETRY_ON_ERROR_BACKOFF);
            }
        }
    }
    // Every attempt came back empty — the item DISPATCHED (ok), produced no
    // usable content, and its whole spend is billed (the reasoning-guillotine
    // case the probe stage's retry loop already handled).
    MapItemResult {
        index,
        ok: true,
        content: String::new(),
        error: None,
        total_tokens: item_total_tokens(any_usage, sum),
        prompt_tokens: item_split_tokens(any_split, psum),
        completion_tokens: item_split_tokens(any_split, csum),
        served_model: None,
        wall_ms,
        retried: error_retries_used,
    }
}

/// (#1442) One HOSTED map item — the remote-bucketed sibling of
/// [`map_local_item`]. Each attempt (including a `retry_on_empty` or
/// `retry_on_error`, #1605, retry) draws from the SHARED per-execution
/// bucket: it admits before the call, clamps `max_tokens` to what remains
/// (#1442 gate C6), and spends the conservative cost after. A first-attempt
/// exhaustion is the named skip (`ok: false`); a LATER-attempt exhaustion
/// stops retrying and keeps the empty-but-dispatched result already earned
/// (never a spurious skip for an item that did fire).
#[allow(clippy::too_many_arguments)]
fn map_hosted_item(
    index: usize,
    bucket: &Arc<Mutex<MapRemoteBucket>>,
    endpoint: &darkmux_types::ModelEndpoint,
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    timeout_seconds: u32,
    retry_on_empty: u32,
    retry_on_error: u32,
    ovr: Option<&MapDispatchOverride>,
) -> MapItemResult {
    use crate::single_shot::HostedSingleShotRequest;
    let mut sum = 0u64;
    let mut any_usage = false;
    // (#1530 dogfood) Parallel to `sum`/`any_usage`, for the usage SPLIT.
    let (mut psum, mut csum, mut any_split) = (0u64, 0u64, false);
    // (#1442) Cumulative dispatch wall-clock across every attempt (the same
    // shape as `sum`), and the ENDPOINT-reported served model — captured from
    // the reply body's `model` field (last non-`None` across attempts wins, so
    // a later usage-less reply never erases a served model an earlier attempt
    // reported). A first-attempt budget skip fires no call, so both stay at
    // their honest zero/`None`.
    let mut wall_ms = 0u64;
    let mut served_model: Option<String> = None;
    let mut empty_budget = retry_on_empty;
    let mut error_budget = retry_on_error;
    let mut error_retries_used = 0u32;
    let mut attempt: u32 = 0;
    loop {
        // (#1442 fan-out) admit_reserve grants — and RESERVES — the clamped
        // completion cap in one locked operation, so concurrent sibling
        // steps sharing this bucket (`bucket_group`) cannot all admit
        // against the same untouched balance; see the method's own doc.
        let granted = bucket
            .lock()
            .expect("map remote bucket mutex poisoned")
            .admit_reserve(max_tokens);
        let Some(clamped) = granted else {
            if attempt == 0 {
                return MapItemResult {
                    index,
                    ok: false,
                    content: String::new(),
                    error: Some(MAP_BUDGET_SKIP_ERROR.to_string()),
                    total_tokens: None,
                    // No call fired, so there is no split to report either.
                    prompt_tokens: None,
                    completion_tokens: None,
                    served_model: None,
                    wall_ms,
                    retried: 0,
                };
            }
            // A retry the bucket can no longer fund — stop, keep what fired.
            break;
        };
        let req = HostedSingleShotRequest {
            endpoint,
            model,
            system,
            user,
            max_tokens: clamped,
            timeout_seconds,
        };
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
                bucket
                    .lock()
                    .expect("map remote bucket mutex poisoned")
                    .settle(clamped, conservative_hosted_spend(reply.total_tokens, clamped));
                if let Some(t) = reply.total_tokens {
                    sum += t;
                    any_usage = true;
                }
                accumulate_split(
                    reply.prompt_tokens,
                    reply.completion_tokens,
                    &mut psum,
                    &mut csum,
                    &mut any_split,
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
                        total_tokens: item_total_tokens(any_usage, sum),
                        prompt_tokens: item_split_tokens(any_split, psum),
                        completion_tokens: item_split_tokens(any_split, csum),
                        served_model,
                        wall_ms,
                        retried: error_retries_used,
                    };
                }
                // Empty content — retry (if the bucket funds another attempt).
                if empty_budget == 0 {
                    break;
                }
                empty_budget -= 1;
            }
            // (#1605) See `map_local_item`'s matching arm — same policy:
            // retried only when `retry_on_error` opted in, with a short
            // backoff and `error_retries_used` tracking how many fired.
            Err(e) => {
                // Release the reservation — a dispatch-level error spent
                // nothing (the pre-reserve accounting billed 0 here too).
                bucket.lock().expect("map remote bucket mutex poisoned").settle(clamped, 0);
                if error_budget == 0 {
                    return MapItemResult {
                        index,
                        ok: false,
                        content: String::new(),
                        error: Some(format!("{e:#}")),
                        total_tokens: item_total_tokens(any_usage, sum),
                        prompt_tokens: item_split_tokens(any_split, psum),
                        completion_tokens: item_split_tokens(any_split, csum),
                        served_model,
                        wall_ms,
                        retried: error_retries_used,
                    };
                }
                error_budget -= 1;
                error_retries_used += 1;
                std::thread::sleep(RETRY_ON_ERROR_BACKOFF);
            }
        }
        attempt += 1;
    }
    MapItemResult {
        index,
        ok: true,
        content: String::new(),
        error: None,
        total_tokens: item_total_tokens(any_usage, sum),
        prompt_tokens: item_split_tokens(any_split, psum),
        completion_tokens: item_split_tokens(any_split, csum),
        served_model,
        wall_ms,
        retried: error_retries_used,
    }
}

impl StepKind for DispatchMapStepKind {
    fn id(&self) -> &'static str {
        "dispatch.map"
    }

    fn display_name(&self) -> &'static str {
        "Dispatch (map)"
    }

    fn run(&self, step: &Step, _task: &Task, input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        // Ctx-free path (unit tests / callers with no scheduler seam): every
        // record batches into `StepOutcome.flow_records`, and the remote
        // bucket is step-scoped (no `bucket_group` sharing without a
        // scheduler to own the group map).
        self.run_map(step, input, None)
    }

    /// (#1442 gate C3) The scheduler entry point — LIVE per-item emission
    /// through the [`StepRunCtx`] channel (so a 30-item map lands items on
    /// the graph page as they finish, never batched at wave-drain) and the
    /// scheduler-supplied shared `bucket_group` bucket when the step names
    /// one. Delegates to the same [`Self::run_map`] body the ctx-free `run`
    /// uses, differing ONLY in where records go and which bucket meters.
    fn run_streaming(
        &self,
        step: &Step,
        _task: &Task,
        input: &BTreeMap<String, String>,
        ctx: &StepRunCtx,
    ) -> Result<StepOutcome> {
        self.run_map(step, input, Some(ctx))
    }

    /// (#1442) LOCAL residency hint. Returns `None` — no wave load — when:
    /// the step is HOSTED (`endpoint` present, nothing local to load); the
    /// collection is EMPTY (the short-circuit: a guaranteed no-op needs no
    /// model, ported generically from the review verify seat, #1442); or the
    /// residency hints (`n_ctx`) are absent (fail-open, like
    /// `resolve_local_placement` — a missed RAM-safety optimization, never a
    /// hard failure). The empty-collection `None` is the mechanism by which
    /// an empty map performs ZERO model loads — the property the block-level
    /// short-circuit guarantees.
    fn residency(
        &self,
        step: &Step,
        _task: &Task,
        input: &BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> Option<darkmux_gestalt::Placement> {
        if step.config.get("endpoint").is_some() {
            return None;
        }
        match resolve_map_collection(step, input) {
            Ok(items) if items.is_empty() => return None,
            Ok(_) => {}
            // `run` owns surfacing a malformed collection; residency stays a
            // best-effort classification that must never mask it.
            Err(_) => return None,
        }
        let model = config_str(step, "model")?;
        let min_ctx = u32::try_from(step.config.get("n_ctx").and_then(|v| v.as_u64())?).ok()?;
        let identifier = config_str(step, "identifier")
            .map(str::to_string)
            .unwrap_or_else(|| darkmux_gestalt::namespaced_identifier(model, None));
        // (#1442 ship-2b) `model_key` — the LOADABLE model key when it
        // differs from the wire `model` id. A local seat dispatches against
        // its darkmux-NAMESPACED identifier (`darkmux:<id>` as the wire
        // `model`), but the wave loader's `lms load` needs the bare model
        // key; without this override the loader would try to load the
        // namespaced string as if it were a model key.
        let model_key = config_str(step, "model_key").unwrap_or(model);
        Some(darkmux_gestalt::Placement {
            model_key: model_key.to_string(),
            identifier,
            min_ctx,
            // (#1442 gate C7) "step:<id>", consistent with the placement
            // provenance `dispatch.internal`'s residency uses.
            seat: format!("step:{}", step.id),
        })
    }
}

/// Runs a shell command from `Step.config`. Required: `command`
/// (string, passed to `sh -c`). Optional: `cwd` (string). Every
/// dependency's output is exposed as an env var
/// `DARKMUX_STEP_INPUT_<SANITIZED-DEP-ID>` (non-alphanumeric bytes in
/// the dependency id become `_`) so a shell step can consume prior
/// output without darkmux having to parse the command's own
/// substitution syntax. A non-zero exit is a loud `Err` carrying stdout
/// + stderr, never a silently-`Ok` failed command.
pub struct ProceduralShellStepKind;

impl StepKind for ProceduralShellStepKind {
    fn id(&self) -> &'static str {
        "procedural.shell"
    }

    fn display_name(&self) -> &'static str {
        "Shell"
    }

    fn run(&self, step: &Step, _task: &Task, input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let command = require_config_str(step, self.id(), "command")?;
        let cwd = config_str(step, "cwd");

        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        for (dep_id, output) in input {
            let env_key = sanitize_env_key(dep_id);
            cmd.env(format!("DARKMUX_STEP_INPUT_{env_key}"), output);
        }

        let out = cmd
            .output()
            .with_context(|| format!("step `{}`: spawning shell command", step.id))?;
        if !out.status.success() {
            anyhow::bail!(
                "step `{}`: command exited with {:?}\nstdout: {}\nstderr: {}",
                step.id,
                out.status.code(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        Ok(StepOutcome {
            output: String::from_utf8_lossy(&out.stdout).to_string(),
            flow_records: Vec::new(),
        })
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
    fn id(&self) -> &'static str {
        "procedural.noop"
    }

    fn display_name(&self) -> &'static str {
        "No-op"
    }

    fn run(&self, step: &Step, _task: &Task, _input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let output = config_str(step, "output").unwrap_or(&step.id).to_string();
        Ok(StepOutcome {
            output,
            flow_records: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(id: &str, kind: &str, config: serde_json::Value) -> Step {
        Step {
            id: id.to_string(),
            task_id: "t1".to_string(),
            kind: kind.to_string(),
            status: crate::types::NodeStatus::Planned,
            config,
            started_ts: None,
            completed_ts: None,
            output: None,
        }
    }

    /// A Task with no resource assignment (#1230/#1341) — the default
    /// fixture for tests that don't exercise Task-sourced
    /// `role_id`/`profile_name`/`workdir`/`image` (see
    /// `dispatch_internal_sources_role_id_from_task` for a test that
    /// does).
    fn empty_task() -> Task {
        Task {
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

    /// (#1530 Packet 3a) A bare `StepRunCtx` — no emitter/bucket/override,
    /// an empty `ArtifactBus` — for tests that call `residency()` directly
    /// (bypassing the scheduler, which is the only production caller that
    /// materializes a real bus). None of `residency()`'s Tier 1 builtin
    /// implementations read the bus, so an empty one is sufficient here.
    fn bare_ctx() -> StepRunCtx {
        StepRunCtx::new(None, None, None, std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()))
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
            session_id: "crew-dispatch-coder-123-0".to_string(),
            out_dir: Some(std::path::PathBuf::from("/tmp/darkmux-out")),
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: RawDispatchOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back.exit_code, 2);
        assert_eq!(back.stdout, "some output");
        assert_eq!(back.stderr, "some warning");
        assert_eq!(back.session_id, "crew-dispatch-coder-123-0");
        assert_eq!(back.out_dir, Some(std::path::PathBuf::from("/tmp/darkmux-out")));
    }

    #[test]
    fn raw_dispatch_outcome_out_dir_omitted_when_none() {
        let original = RawDispatchOutcome {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            session_id: "s".to_string(),
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
        let err = DispatchInternalStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("config.role_id"), "{err}");
    }

    #[test]
    fn dispatch_single_shot_requires_model() {
        let s = step("s1", "dispatch.single_shot", json!({"user": "hi"}));
        let err = DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("config.model"), "{err}");
    }

    #[test]
    fn procedural_shell_requires_command() {
        let s = step("s1", "procedural.shell", json!({}));
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("config.command"), "{err}");
    }

    #[test]
    fn procedural_shell_runs_and_captures_stdout() {
        let s = step("s1", "procedural.shell", json!({"command": "echo hello-shell"}));
        let out = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        assert!(out.output.contains("hello-shell"));
    }

    #[test]
    fn procedural_shell_nonzero_exit_is_an_error() {
        let s = step("s1", "procedural.shell", json!({"command": "exit 3"}));
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("exited with"), "{err}");
    }

    #[test]
    fn procedural_shell_exposes_dependency_output_as_env_var() {
        let mut input = BTreeMap::new();
        input.insert("upstream-step".to_string(), "value-from-upstream".to_string());
        let s = step(
            "s1",
            "procedural.shell",
            json!({"command": "echo $DARKMUX_STEP_INPUT_UPSTREAM_STEP"}),
        );
        let out = ProceduralShellStepKind.run(&s, &empty_task(), &input).unwrap();
        assert!(out.output.contains("value-from-upstream"), "got: {}", out.output);
    }

    #[test]
    fn procedural_noop_defaults_output_to_step_id() {
        let s = step("marker-step", "procedural.noop", json!(null));
        let out = ProceduralNoopStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        assert_eq!(out.output, "marker-step");
    }

    #[test]
    fn procedural_noop_honors_config_output_override() {
        let s = step("s1", "procedural.noop", json!({"output": "custom"}));
        let out = ProceduralNoopStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
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
        let err = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("config.model"), "{err}");
    }

    #[test]
    fn dispatch_map_requires_user_template_once_the_collection_is_non_empty() {
        let s = map_step(json!({ "model": "m", "collection": ["a"] }));
        let err = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("config.user_template"), "{err}");
    }

    #[test]
    fn map_item_text_substitutes_strings_verbatim_and_json_compactly() {
        assert_eq!(map_item_text(&json!("hello")), "hello");
        assert_eq!(map_item_text(&json!({ "id": "b1" })), r#"{"id":"b1"}"#);
        assert_eq!(map_item_text(&json!(42)), "42");
    }

    #[test]
    fn resolve_map_collection_prefers_config_collection() {
        let s = map_step(json!({ "collection": ["a", "b", "c"] }));
        let out = resolve_map_collection(&s, &BTreeMap::new()).unwrap();
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn resolve_map_collection_reads_the_single_dependency_input_as_a_json_array() {
        let s = map_step(json!({}));
        let mut input = BTreeMap::new();
        input.insert("upstream".to_string(), r#"["x","y"]"#.to_string());
        let out = resolve_map_collection(&s, &input).unwrap();
        assert_eq!(out, vec![json!("x"), json!("y")]);
    }

    #[test]
    fn resolve_map_collection_reads_the_named_collection_input() {
        let s = map_step(json!({ "collection_input": "bundles" }));
        let mut input = BTreeMap::new();
        input.insert("bundles".to_string(), r#"["only-this"]"#.to_string());
        input.insert("other".to_string(), r#"["ignored"]"#.to_string());
        let out = resolve_map_collection(&s, &input).unwrap();
        assert_eq!(out, vec![json!("only-this")]);
    }

    #[test]
    fn resolve_map_collection_truly_absent_or_blank_source_is_an_empty_collection() {
        // No config.collection, no collection_input, ZERO inputs — the one
        // genuinely collection-less shape that stays a real, silent zero.
        let s = map_step(json!({}));
        assert!(resolve_map_collection(&s, &BTreeMap::new()).unwrap().is_empty());
        // A present-but-blank input string is a real zero too (the upstream
        // truly produced nothing), not an error.
        let mut input = BTreeMap::new();
        input.insert("u".to_string(), "   ".to_string());
        assert!(resolve_map_collection(&s, &input).unwrap().is_empty());
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
        let err = resolve_map_collection(&s, &two).unwrap_err();
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
        let err = resolve_map_collection(&s, &input).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("`bundles`"), "the missing key is named: {msg}");
        assert!(msg.contains("upstream"), "the present inputs are named: {msg}");

        // Zero inputs at all: same loud error, with "none" as the roster.
        let err = resolve_map_collection(&s, &BTreeMap::new()).unwrap_err();
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
        let err = resolve_map_collection(&s, &input).unwrap_err();
        assert!(
            err.to_string().contains("config.collection must be a JSON array"),
            "{err}"
        );
    }

    #[test]
    fn resolve_map_collection_non_array_source_is_a_loud_error() {
        let s = map_step(json!({}));
        let mut input = BTreeMap::new();
        input.insert("u".to_string(), r#"{"not":"an array"}"#.to_string());
        let err = resolve_map_collection(&s, &input).unwrap_err();
        assert!(err.to_string().contains("must be a JSON array"), "{err}");
    }

    #[test]
    fn dispatch_map_empty_collection_short_circuits_without_dispatch() {
        // The block-level short-circuit (#1442): an empty collection returns
        // an empty `[]` output with a named short-circuit record, and NEVER
        // reaches the model/user_template requirements or any dispatch — so a
        // config missing `model` still succeeds here (nothing to dispatch).
        let s = map_step(json!({ "collection": [] }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        assert_eq!(out.output, "[]");
        assert_eq!(out.flow_records.len(), 1, "one short-circuit record");
        let payload = out.flow_records[0].payload.as_ref().unwrap();
        assert!(payload["short_circuit"].as_str().unwrap().contains("empty collection"));
    }

    #[test]
    fn dispatch_map_empty_collection_residency_is_none_so_no_model_loads() {
        // The no-load property: even with a full local residency config
        // (model + n_ctx present), an EMPTY collection makes residency return
        // None, so the wave loader is never asked to load a model the map
        // won't use. This is the #1442 empty-docket short-circuit, generic.
        let s = map_step(json!({
            "model": "some-local-model",
            "user_template": "check {item}",
            "n_ctx": 8192,
            "collection": [],
        }));
        assert!(
            DispatchMapStepKind.residency(&s, &empty_task(), &BTreeMap::new(), &bare_ctx()).is_none(),
            "an empty collection must declare no residency need (no load)"
        );
    }

    #[test]
    fn dispatch_map_local_residency_resolves_a_placement_for_a_non_empty_collection() {
        let s = map_step(json!({
            "model": "qwen3.6-35b-a3b",
            "user_template": "check {item}",
            "n_ctx": 8192,
            "collection": ["a"],
        }));
        let placement = DispatchMapStepKind.residency(&s, &empty_task(), &BTreeMap::new(), &bare_ctx()).unwrap();
        assert_eq!(placement.model_key, "qwen3.6-35b-a3b");
        assert_eq!(placement.min_ctx, 8192);
        assert!(placement.identifier.starts_with("darkmux:"), "default identifier is namespaced: {}", placement.identifier);
        // (#1442 gate C7) "step:<id>", consistent with dispatch.internal's
        // placement provenance.
        assert_eq!(placement.seat, "step:m1");
    }

    #[test]
    fn dispatch_map_hosted_residency_is_none() {
        // An endpoint-bearing (remote) map loads nothing locally.
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "n_ctx": 8192,
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
        }));
        assert!(DispatchMapStepKind.residency(&s, &empty_task(), &BTreeMap::new(), &bare_ctx()).is_none());
    }

    #[test]
    fn dispatch_map_residency_none_without_n_ctx_fails_open() {
        let s = map_step(json!({ "model": "m", "user_template": "u {item}", "collection": ["a"] }));
        assert!(
            DispatchMapStepKind.residency(&s, &empty_task(), &BTreeMap::new(), &bare_ctx()).is_none(),
            "no n_ctx hint -> None (fail open to Remote scheduling), never a hard failure"
        );
    }

    #[test]
    fn map_remote_bucket_admits_until_exhausted_then_skips() {
        // (#1617 review) Magnitudes scaled x1000 from the original 100/60.
        // Those were arbitrary small numbers chosen to exercise the ARITHMETIC,
        // but they sit below `MIN_VIABLE_MAP_GRANT` — so once the starvation
        // floor landed, this test was measuring the floor instead of the
        // accounting it exists to pin. Same shape, realistic token counts.
        let mut b = MapRemoteBucket::new(100_000);
        let g1 = b.admit_reserve(60_000).expect("fresh bucket admits");
        b.settle(g1, 60_000);
        let g2 = b.admit_reserve(60_000).expect("still under budget");
        b.settle(g2, 60_000); // now 120k >= 100k (the endpoint reported above its grant)
        assert!(b.admit_reserve(60_000).is_none(), "over budget -> skip");
        assert_eq!(b.skipped(), 1);
    }

    #[test]
    fn map_remote_bucket_reservation_holds_the_grant_until_settled() {
        // (#1442 fan-out) The reserve-then-settle shape: a granted call's cap
        // is held against the budget WHILE the call is in flight, so a
        // concurrent sibling admitting mid-flight sees the reservation —
        // never the untouched balance (the allowance-multiplication race a
        // spend-after pair reintroduces under `seats x k` sibling
        // concurrency).
        let mut b = MapRemoteBucket::new(100);
        let granted = b.admit_reserve(4096).expect("admits");
        assert_eq!(granted, 100, "the grant clamps to what remains");
        assert!(b.admit_reserve(10).is_none(), "an in-flight reservation blocks siblings");
        assert_eq!(b.skipped(), 1);
        // Settling with the real (higher) usage keeps the overshoot honest…
        b.settle(granted, 600);
        assert!(b.exhausted());
        // …and settling an ERRORED call with 0 releases the whole grant.
        let mut b2 = MapRemoteBucket::new(100);
        let g = b2.admit_reserve(4096).expect("admits");
        b2.settle(g, 0);
        assert_eq!(b2.remaining(), 100, "an errored call spends nothing");
    }

    /// (#1610 / #1617 review) The same starvation class the judge bucket was
    /// floored against, which this bucket carried unfloored.
    ///
    /// A grant too small to hold a reply "succeeds", the endpoint truncates
    /// mid-JSON, and the caller reads the debris as a result. On this bucket —
    /// the `dispatch.map` fan-out, i.e. the probe stage — that is reduced
    /// COVERAGE reported as a clean run. A low-flag review must mean "few
    /// flags", never "we stopped looking and said nothing".
    #[test]
    fn map_remote_bucket_denies_a_starved_grant_instead_of_truncating() {
        // A budget that was never small, spent down to a sliver: a probe asks
        // for a usable cap and would be handed 40 tokens. That is the failure.
        let mut b = MapRemoteBucket::new(100_000);
        let g = b.admit_reserve(99_960).expect("the first draw admits");
        b.settle(g, 99_960);
        assert_eq!(b.remaining(), 40);
        assert!(
            b.admit_reserve(4096).is_none(),
            "a 40-token grant cannot hold a reply — deny it rather than truncate"
        );
        assert_eq!(b.skipped(), 1, "and COUNT it, or the run reports coverage it never had");

        // Not starvation: the operator configured a tiny BUDGET. That is
        // policy — the only documented refusal value for the knob is 0, so a
        // floor that swallowed small budgets would invent a second one.
        let mut tiny = MapRemoteBucket::new(100);
        assert_eq!(
            tiny.admit_reserve(4096),
            Some(100),
            "a deliberately small budget is operator policy, not a starved grant"
        );
        assert_eq!(tiny.skipped(), 0);

        // A healthy bucket grants the full ask untouched — the floor must be
        // invisible on the path that matters most.
        let mut healthy = MapRemoteBucket::new(100_000);
        assert_eq!(healthy.admit_reserve(4096), Some(4096));
        assert_eq!(healthy.skipped(), 0);
    }

    #[test]
    fn map_remote_bucket_zero_budget_is_exhausted_from_the_first_item() {
        // The hard opt-out: a 0 allowance refuses every hosted call, the same
        // as `admit_remote_execution` refuses a single hosted dispatch.
        let mut b = MapRemoteBucket::new(0);
        assert!(b.admit_reserve(10).is_none(), "zero budget admits nothing");
        assert!(b.exhausted());
    }

    #[test]
    fn map_remote_bucket_remaining_shrinks_with_spend_and_never_underflows() {
        // (#1442 gate C6) The per-item clamp target: what is LEFT, not the
        // full budget — a late item must not be granted more than remains.
        // (#1617 review) Scaled x1000 from 100/70/30 for the same reason as
        // `..._admits_until_exhausted_then_skips` above: the original numbers
        // are below the starvation floor, so they would exercise it rather
        // than the clamp arithmetic this test is about.
        let mut b = MapRemoteBucket::new(100_000);
        assert_eq!(b.remaining(), 100_000);
        let g = b.admit_reserve(70_000).expect("admits");
        b.settle(g, 70_000);
        assert_eq!(b.remaining(), 30_000, "a later item's grant clamps to 30k, not 100k");
        assert_eq!(
            b.admit_reserve(40_960).expect("still admits"),
            30_000,
            "the grant reads the remaining allowance"
        );
        b.settle(30_000, 60_000); // overshoot: the endpoint reported above its grant
        assert_eq!(b.remaining(), 0, "saturating, never an underflow wrap");
    }

    #[test]
    fn conservative_hosted_spend_charges_the_granted_cap_when_usage_is_omitted() {
        // (#1442 gate C4) A reply that reports usage spends what it reports;
        // a reply that OMITS usage spends the clamped max_tokens it was
        // granted — an omitting endpoint must not mint an infinite allowance.
        assert_eq!(conservative_hosted_spend(Some(1234), 4096), 1234);
        assert_eq!(conservative_hosted_spend(None, 4096), 4096);
        assert_eq!(conservative_hosted_spend(None, 0), 0);
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
    /// Asserting the SHAPE rather than a literal: the point is that the id
    /// comes from `session_id::task`, so a future rename moves both producer
    /// and consumer together instead of silently re-breaking the meters.
    #[test]
    fn map_emits_canonical_task_session_ids_not_colon_form() {
        let expected = darkmux_types::session_id::task("t1");
        assert!(
            !expected.contains(':'),
            "canonical form must not be colon-delimited, got {expected}"
        );
        assert_eq!(expected, "task-t1", "the hyphen form mission_graph folds on");

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
            served_model: None,
            wall_ms: 0,
            retried: 0,
        }];
        let rec = DispatchMapStepKind::aggregate_record(&s, "m", false, &results);
        assert_eq!(
            rec.session_id.as_deref(),
            Some(expected.as_str()),
            "aggregate record must carry the canonical id or the graph meters stay blank"
        );
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
            served_model: None,
            wall_ms: 0,
            retried: 0,
        };
        let payload = map_item_token_payload(&res).expect("a reply with usage emits a record");
        assert_eq!(payload["total_tokens"], 4547);
        assert_eq!(payload["prompt_tokens"], 2490, "GENERATED/fresh/re-read read the split");
        assert_eq!(payload["completion_tokens"], 2057);
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
            served_model: None,
            wall_ms: 0,
            retried: 0,
        };
        let payload = map_item_token_payload(&res).expect("a total alone still emits");
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
            served_model: None,
            wall_ms: 0,
            retried: 0,
        };
        let payload = map_item_token_payload(&res).expect("a split alone still emits");
        assert_eq!(payload["total_tokens"], 42, "arithmetic on reported parts, not fabrication");
        assert_eq!(payload["prompt_tokens"], 30);
        assert_eq!(payload["completion_tokens"], 12);
    }

    /// An item that reported no usage at all emits NO record (pre-existing
    /// behavior, pinned here so the payload refactor didn't change it).
    #[test]
    fn map_item_with_no_usage_emits_no_token_record() {
        let res = MapItemResult {
            index: 0,
            ok: false,
            content: String::new(),
            error: Some("boom".to_string()),
            total_tokens: None,
            prompt_tokens: None,
            completion_tokens: None,
            served_model: None,
            wall_ms: 0,
            retried: 0,
        };
        assert!(map_item_token_payload(&res).is_none());
    }

    /// The accumulator folds multi-attempt usage and keeps `None` honest.
    #[test]
    fn split_accumulates_across_attempts_and_stays_absent_when_never_reported() {
        let (mut p, mut c, mut any) = (0u64, 0u64, false);
        accumulate_split(None, None, &mut p, &mut c, &mut any);
        assert!(!any, "a reply with no split must not arm the flag");
        assert_eq!(item_split_tokens(any, p), None);

        accumulate_split(Some(10), Some(4), &mut p, &mut c, &mut any);
        accumulate_split(Some(7), Some(3), &mut p, &mut c, &mut any);
        assert_eq!(item_split_tokens(any, p), Some(17));
        assert_eq!(item_split_tokens(any, c), Some(7));

        // A half-reported split still counts, with the missing half as 0 —
        // the same "partial is better than nothing" rule `total` uses.
        let (mut p2, mut c2, mut any2) = (0u64, 0u64, false);
        accumulate_split(Some(5), None, &mut p2, &mut c2, &mut any2);
        assert_eq!(item_split_tokens(any2, p2), Some(5));
        assert_eq!(item_split_tokens(any2, c2), Some(0));
    }

    #[test]
    fn dispatch_map_aggregate_record_sums_tokens_and_counts_outcomes() {
        // (#1442 gate C1) The one step-level aggregate: items_in, ok_count,
        // failed_count, remote, and SUMMED total_tokens — the record the
        // mission graph's max-fold token meter reads as the step's true
        // spend (any per-item value is <= the sum).
        let results = vec![
            MapItemResult { index: 0, ok: true, content: "a".to_string(), error: None, total_tokens: Some(100), prompt_tokens: None, completion_tokens: None, served_model: None, wall_ms: 0, retried: 0 },
            MapItemResult { index: 1, ok: false, content: String::new(), error: Some("boom".to_string()), total_tokens: None, prompt_tokens: None, completion_tokens: None, served_model: None, wall_ms: 0, retried: 0 },
            MapItemResult { index: 2, ok: true, content: "c".to_string(), error: None, total_tokens: Some(250), prompt_tokens: None, completion_tokens: None, served_model: None, wall_ms: 0, retried: 0 },
        ];
        let s = map_step(json!({}));
        let rec = DispatchMapStepKind::aggregate_record(&s, "m", true, &results);
        let p = rec.payload.as_ref().unwrap();
        assert_eq!(p["kind"], "dispatch.map");
        assert_eq!(p["items_in"], 3);
        assert_eq!(p["ok_count"], 2);
        assert_eq!(p["failed_count"], 1);
        assert_eq!(p["remote"], true);
        assert_eq!(p["total_tokens"], 350, "summed across items, absent usage counted as 0 here");
        assert!(
            matches!(rec.level, darkmux_flow::Level::Warn),
            "any failed item raises the level"
        );

        let clean = vec![MapItemResult { index: 0, ok: true, content: "a".to_string(), error: None, total_tokens: Some(5), prompt_tokens: None, completion_tokens: None, served_model: None, wall_ms: 0, retried: 0 }];
        let rec = DispatchMapStepKind::aggregate_record(&s, "m", false, &clean);
        assert!(matches!(rec.level, darkmux_flow::Level::Info));
        assert_eq!(rec.payload.as_ref().unwrap()["remote"], false);
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_local_per_item_error_isolation_continues_past_a_failure() {
        // Point the local dialect at an unroutable endpoint (port 1 refuses
        // immediately, 1s timeout) so EVERY item's dispatch errors — the
        // policy under test is that each failure is CAPTURED into that item's
        // result and the loop CONTINUES to the next, rather than the first
        // error aborting the whole step. Three items in -> three ok:false
        // results out, step still Ok.
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
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 3, "every item produced a result despite each failing");
        assert!(results.iter().all(|r| !r.ok), "each item's dispatch failed and was isolated");
        assert!(results.iter().all(|r| r.error.is_some()), "each failure named");
        assert_eq!(results[0].index, 0);
        assert_eq!(results[2].index, 2);
        // A per-item flow record for every item, PLUS the one step-level
        // aggregate after the loop (#1442 gate C1) — 3 + 1.
        assert_eq!(out.flow_records.len(), 4);
        let agg = out.flow_records.last().unwrap().payload.as_ref().unwrap();
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
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
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

    #[test]
    #[serial_test::serial]
    fn dispatch_map_hosted_bucket_exhaustion_mid_collection_skips_remaining_items() {
        // Budget 0 (the hard opt-out) exhausts from the first item, so every
        // hosted item is SKIPPED with the named budget reason — no HTTP call
        // fires (proven by the distinct skip message, not a connect error).
        // This exercises the mid-collection exhaustion policy at its edge: the
        // whole collection is skipped, each item recording the same reason.
        let budget_key = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(budget_key).ok();
        unsafe {
            std::env::set_var(budget_key, "0");
        }
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a", "b", "c"],
            "endpoint": { "url": "http://127.0.0.1:1" },
            "timeout_seconds": 1,
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| !r.ok));
        for r in &results {
            let msg = r.error.as_deref().unwrap();
            assert!(msg.contains("remote token budget exhausted"), "budget skip named: {msg}");
            assert!(
                !msg.to_lowercase().contains("connect") && !msg.to_lowercase().contains("curl"),
                "no HTTP call was attempted (skipped before the network): {msg}"
            );
        }
        unsafe {
            match prev {
                Some(v) => std::env::set_var(budget_key, v),
                None => std::env::remove_var(budget_key),
            }
        }
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
            total_tokens: total,
            prompt_tokens: None,
            completion_tokens: None,
            model: Some("hosted".to_string()),
        }
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_hosted_partial_exhaustion_mid_collection() {
        // item 1 spends the WHOLE 100-token allowance, so the SHARED bucket
        // is exhausted for items 2 and 3 — they skip with the named reason.
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe {
            std::env::set_var(k, "100");
        }
        clear_hosted_override();
        install_hosted_override(|_req| Ok(hosted_reply(Some(100))));
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a", "b", "c"],
            "endpoint": { "url": "https://example.com" },
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 3);
        assert!(results[0].ok, "item 1 dispatched and succeeded");
        assert_eq!(results[0].total_tokens, Some(100), "item 1 reported its real usage");
        for r in &results[1..] {
            assert!(!r.ok, "item {} skipped after exhaustion", r.index);
            let msg = r.error.as_deref().unwrap();
            assert!(msg.contains("remote token budget exhausted"), "named skip reason: {msg}");
            // (#1442 gate) A skipped item reports HONEST None — never a
            // fabricated 0 that a run-level token sum would silently swallow.
            assert_eq!(r.total_tokens, None, "skipped item's total_tokens stays honest None");
        }
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_hosted_reply_without_usage_stays_honest_none_at_run_level() {
        // An endpoint that omits usage entirely: the item is `ok` (it
        // dispatched) but its `total_tokens` is honest `None` — the run-level
        // result array never fabricates a number the endpoint didn't send.
        // (The bucket still charges the conservative clamped grant so an
        // omitting endpoint can't run the whole collection off the meter.)
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe {
            std::env::set_var(k, "500000");
        }
        clear_hosted_override();
        install_hosted_override(|_req| Ok(hosted_reply(None)));
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a", "b"],
            "endpoint": { "url": "https://example.com" },
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.ok), "both dispatched");
        assert!(
            results.iter().all(|r| r.total_tokens.is_none()),
            "no fabricated token count when the endpoint omitted usage"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
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
                total_tokens: total,
                prompt_tokens: None,
                completion_tokens: None,
                model: Some("hosted".to_string()),
            })
        });
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_retry_on_empty_retries_then_succeeds() {
        // First attempt returns empty (but bills 50), the retry returns real
        // content (bills 70). retry_on_empty=1 → the item ends ok with the
        // non-empty content and tokens SUMMED across both attempts.
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe {
            std::env::set_var(k, "500000");
        }
        clear_hosted_override();
        install_scripted_hosted(vec![("", Some(50)), ("flag", Some(70))]);
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
            "retry_on_empty": 1,
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].ok, "the retry produced usable content");
        assert_eq!(results[0].content, "flag");
        assert_eq!(results[0].total_tokens, Some(120), "tokens billed across BOTH attempts");
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_retry_on_empty_gives_up_honestly() {
        // Both attempts empty (bill 50 + 60). retry_on_empty=1 exhausts, and
        // the item ends ok:true with EMPTY content (dispatched, no usable
        // result) and the full spend billed — never a flag from nothing.
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe {
            std::env::set_var(k, "500000");
        }
        clear_hosted_override();
        install_scripted_hosted(vec![("", Some(50)), ("   ", Some(60))]);
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
            "retry_on_empty": 1,
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].ok, "it dispatched — the empty content is a real, honest zero");
        assert!(results[0].content.is_empty(), "no usable content after the retries");
        assert_eq!(results[0].total_tokens, Some(110), "every attempt's spend billed");
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_retry_on_empty_default_off_accepts_the_first_empty_reply() {
        // With no retry_on_empty configured (default 0), an empty reply is
        // accepted as-is on the FIRST attempt — one call, tokens from it only.
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe {
            std::env::set_var(k, "500000");
        }
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
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].ok);
        assert!(results[0].content.is_empty(), "default off does not retry the empty reply");
        assert_eq!(results[0].total_tokens, Some(40), "exactly one call was made");
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
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
                    total_tokens: *total,
                    prompt_tokens: None,
                    completion_tokens: None,
                    model: Some("hosted".to_string()),
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
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe {
            std::env::set_var(k, "500000");
        }
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
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].ok, "the retry recovered the item");
        assert_eq!(results[0].content, "flag");
        assert_eq!(results[0].retried, 1, "exactly one error-retry was consumed");
        assert_eq!(calls.get(), 2, "exactly two calls fired: the failed attempt + the one retry");
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_retry_on_error_bounded_to_one_never_retries_twice() {
        // Every attempt errors. retry_on_error=1 permits exactly ONE retry —
        // two calls total, then the item isolates as ok:false carrying the
        // LAST attempt's error. A THIRD call would mean the bound leaked.
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe {
            std::env::set_var(k, "500000");
        }
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
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
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
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_retry_on_error_default_off_isolates_immediately() {
        // With no `retry_on_error` configured (default 0/off — the ORIGINAL
        // policy, preserved for every caller that doesn't opt in), a single
        // dispatch error isolates on the FIRST attempt — exactly one call,
        // matching pre-#1605 behavior byte-for-byte.
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe {
            std::env::set_var(k, "500000");
        }
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
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        clear_hosted_override();
        let results: Vec<MapItemResult> = serde_json::from_str(&out.output).unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].ok);
        assert_eq!(results[0].retried, 0, "no retry budget — never retried");
        assert_eq!(calls.get(), 1, "exactly one call — the historical no-retry-on-error behavior");
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn dispatch_map_retry_on_error_out_of_range_is_a_loud_config_error() {
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "retry_on_error": u64::from(u32::MAX) + 1,
        }));
        let err = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
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
        let err = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
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
        let err = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
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
                total_tokens: total,
                prompt_tokens: None,
                completion_tokens: None,
                model: served.map(str::to_string),
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
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe {
            std::env::set_var(k, "500000");
        }
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
        // The batched `run()` path deliberately emits none (see MapBookend).
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = StepRunCtx::new(
            Some(tx),
            None,
            None,
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );
        DispatchMapStepKind
            .run_streaming(&s, &empty_task(), &BTreeMap::new(), &ctx)
            .unwrap();
        drop(ctx);
        clear_hosted_override();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }

        let emitted: Vec<darkmux_flow::FlowRecord> = rx
            .into_iter()
            .filter_map(|sig| match sig {
                crate::step_kinds::WaveSignal::Record(r) => Some(r),
                _ => None,
            })
            .collect();
        let actions: Vec<&str> = emitted.iter().map(|r| r.action.as_str()).collect();
        assert!(
            actions.contains(&"dispatch start"),
            "a hosted map must OPEN its liveness edge; got {actions:?}"
        );
        assert!(
            actions.contains(&"dispatch complete"),
            "...and close it; got {actions:?}"
        );
        // Exactly one of each — the drop guard must not double-emit alongside
        // the clean close.
        assert_eq!(
            actions.iter().filter(|a| **a == "dispatch start").count(),
            1,
            "one start per run; got {actions:?}"
        );
        assert_eq!(
            actions.iter().filter(|a| a.starts_with("dispatch ") && **a != "dispatch start").count(),
            1,
            "exactly one terminal per open; got {actions:?}"
        );

        let terminal = emitted
            .iter()
            .find(|r| r.action == "dispatch complete")
            .expect("terminal present");
        let payload = terminal.payload.as_ref().expect("terminal carries a payload");
        assert_eq!(
            payload["endpoint"], "azure:example.cognitiveservices.azure.com/gpt-4o",
            "the terminal names WHERE the seat ran, in the one format the viewer parses"
        );
        assert_eq!(
            payload["remote_tokens"].as_u64(),
            Some(84),
            "remote spend is the seat's own sum (2 items x 42), matching the aggregate"
        );
        assert_eq!(
            terminal.session_id.as_deref(),
            Some(darkmux_types::session_id::task("t1").as_str()),
            "SAME session as the seat's token records — that join is the whole point"
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
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        for r in &out.flow_records {
            if let Some(p) = r.payload.as_ref() {
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
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe {
            std::env::set_var(k, "500000");
        }
        clear_hosted_override();
        install_hosted_delayed(15, Some("served-model-x"), Some(10));
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
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
        let item_payload = out.flow_records[0].payload.as_ref().unwrap();
        assert_eq!(item_payload["served_model"], "served-model-x");
        assert!(item_payload["wall_ms"].as_u64().unwrap() >= 1);
        let agg = out.flow_records.last().unwrap().payload.as_ref().unwrap();
        assert_eq!(
            agg["total_wall_ms"].as_u64().unwrap(),
            results[0].wall_ms,
            "the aggregate's total_wall_ms is the sum of the per-item walls"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_hosted_item_served_model_is_none_when_the_endpoint_omits_it() {
        // An endpoint that omits `model` yields an honest `None` served_model —
        // never a fabricated empty string and never the requested model echoed
        // back as if served.
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe {
            std::env::set_var(k, "500000");
        }
        clear_hosted_override();
        install_hosted_delayed(0, None, Some(10));
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
        }));
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
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
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
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
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
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
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe {
            std::env::set_var(k, "500000");
        }
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
                    total_tokens: total,
                    prompt_tokens: None,
                    completion_tokens: None,
                    model: Some("served-r".to_string()),
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
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
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
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    // ── (#1412) dispatch.single_shot hosted-arm metering ────────────────

    #[test]
    fn clamp_hosted_max_tokens_never_exceeds_the_budget() {
        assert_eq!(clamp_hosted_max_tokens(4096, 500_000), 4096, "well under budget: unchanged");
        assert_eq!(clamp_hosted_max_tokens(4096, 1_000), 1_000, "clamped down to the budget");
        assert_eq!(
            clamp_hosted_max_tokens(4096, 0),
            0,
            "a zero budget clamps to zero (defensive — unreachable via the admit gate, \
             which already refuses budget 0 before this runs)"
        );
        assert_eq!(
            clamp_hosted_max_tokens(100, u64::MAX),
            100,
            "a budget wider than u32::MAX saturates rather than wrapping, and never inflates a small request"
        );
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_single_shot_hosted_arm_refuses_when_budget_is_zero_before_any_http_call() {
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe {
            std::env::set_var(k, "0");
        }

        // The endpoint URL is deliberately unroutable (port 1 refuses
        // immediately) with a 1s timeout: if the admit gate did NOT fire
        // first, this call would fail with a connection error instead of
        // the budget-exhausted message asserted below. The DISTINCT error
        // text is the proof that `single_shot_chat_hosted` (and therefore
        // the HTTP call) was never reached.
        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({
                "model": "gpt-5.1",
                "user": "hi",
                "endpoint": { "url": "http://127.0.0.1:1" },
                "timeout_seconds": 1,
            }),
        );
        let err = DispatchSingleShotStepKind
            .run(&s, &empty_task(), &BTreeMap::new())
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("remote token budget exhausted"),
            "expected the admit-gate's typed refusal, got: {msg}"
        );
        assert!(
            msg.contains("max_tokens_per_execution"),
            "the error names the exhausted bucket: {msg}"
        );
        assert!(
            !msg.to_lowercase().contains("curl") && !msg.to_lowercase().contains("connect"),
            "no sign of an attempted network call in the error: {msg}"
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_single_shot_local_arm_is_unmetered_by_the_remote_budget() {
        let budget_key = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev_budget = std::env::var(budget_key).ok();
        let prev_url = std::env::var(url_key).ok();
        unsafe {
            // The hard opt-out. If the LOCAL dialect were (wrongly) gated
            // by the remote budget, this would fail with the same
            // "remote token budget exhausted" message the hosted-arm test
            // above asserts on. It must not.
            std::env::set_var(budget_key, "0");
            std::env::set_var(url_key, "http://127.0.0.1:1");
        }

        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({ "model": "some-local-model", "user": "hi", "timeout_seconds": 1 }),
        );
        let err = DispatchSingleShotStepKind
            .run(&s, &empty_task(), &BTreeMap::new())
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("remote token budget exhausted"),
            "the LOCAL dialect must never be gated by DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION: {msg}"
        );
        assert!(
            msg.contains("dispatch.single_shot (local)"),
            "expected the local-arm error context, got: {msg}"
        );

        unsafe {
            match prev_budget {
                Some(v) => std::env::set_var(budget_key, v),
                None => std::env::remove_var(budget_key),
            }
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
    fn tier1_kinds_declare_no_ports_by_default() {
        let kinds: Vec<Arc<dyn StepKind>> = vec![
            Arc::new(DispatchInternalStepKind),
            Arc::new(DispatchSingleShotStepKind),
            Arc::new(DispatchMapStepKind),
            Arc::new(ProceduralShellStepKind),
            Arc::new(ProceduralNoopStepKind),
        ];
        for kind in kinds {
            assert!(
                kind.provides().is_empty(),
                "`{}` should declare no `provides` ports (Tier 1 backward-compat)",
                kind.id()
            );
            assert!(
                kind.requires().is_empty(),
                "`{}` should declare no `requires` ports (Tier 1 backward-compat)",
                kind.id()
            );
        }
    }
}
