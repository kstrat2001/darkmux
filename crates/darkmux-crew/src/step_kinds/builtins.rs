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

use super::types::{StepKind, StepOutcome};
use crate::types::{Step, Task};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;

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
pub fn resolve_local_placement(
    role_id: &str,
    profile_name: Option<&str>,
    config_path: Option<&str>,
    seat: &str,
) -> Option<darkmux_gestalt::Placement> {
    use crate::select::select_model;

    let loaded = darkmux_profiles::profiles::load_registry(config_path).ok()?;
    let (_active_name, profile) = loaded.registry.resolve_active(profile_name)?;

    let roles = crate::loader::load_roles().ok()?;
    let role = roles.iter().find(|r| r.id == role_id)?;

    let skill_index: std::collections::HashMap<String, crate::types::Skill> =
        crate::loader::load_skills()
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect();
    let model_id = select_model(role, profile, |id| skill_index.get(id)).ok()?;
    let pm = profile.models.iter().find(|m| m.id == model_id)?;
    if pm.is_remote() {
        return None;
    }
    let min_ctx = pm.n_ctx?;
    let identifier = darkmux_gestalt::namespaced_identifier(&pm.id, pm.identifier.as_deref());
    Some(darkmux_gestalt::Placement {
        model_key: pm.id.clone(),
        identifier,
        min_ctx,
        seat: seat.to_string(),
    })
}

/// (#1230 Packet 4 DRY pass) One `failed_tool_invocations` entry from the
/// internal runtime's `--json` envelope — a verifier command the dispatched
/// role's tool loop attempted to run but never actually executed (missing
/// binary, toolchain not present, etc). Moved here from `src/mission_run.rs`
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
/// `step:<id>` session id so a caller's own flow records line up with this
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
pub struct DispatchInternalStepKind;

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
        use crate::dispatch::{dispatch, CompactionDispatchArgs, DispatchOpts, Runtime};

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
            .unwrap_or_else(|| format!("step:{}", step.id));
        let parse_verifiers = step
            .config
            .get("parse_verifiers")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let opts = DispatchOpts {
            role_id,
            message,
            deliver: None,
            session_id: Some(session_id),
            timeout_seconds,
            skip_preflight: false,
            json: true,
            workdir,
            phase_id,
            runtime: Runtime::Internal,
            machine: None,
            wait: true,
            compaction: CompactionDispatchArgs::default(),
            profile_name,
            config_path,
            force_container: false,
            max_completion_tokens: None,
            image,
            model_base_url_override: None,
        };
        let result =
            dispatch(opts).with_context(|| format!("step `{}` dispatch.internal", step.id))?;

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
                    session_id: Some(format!("task:{}", step.task_id)),
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
    ) -> Option<darkmux_gestalt::Placement> {
        let role_id = task_or_config_str(task.role_id.as_ref(), step, "role_id")?;
        let profile_name = task_or_config_str(task.profile_name.as_ref(), step, "profile_name");
        let config_path = config_str(step, "config_path").map(str::to_string);
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
                session_id: Some(format!("task:{}", step.task_id)),
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

/// (#1442) One remote-token bucket scoped to a SINGLE `dispatch.map` step's
/// whole collection loop — the step-local analog of the review pipeline's
/// per-stage `RemoteBucket` (`darkmux-lab`'s `review`), which cannot be
/// reused here because `darkmux-lab` depends on `darkmux-crew`, not the
/// reverse (the same dependency-direction note `admit_remote_execution`
/// carries). Local items never touch it. A `budget` of 0 is exhausted from
/// the FIRST item (`used (0) >= budget (0)`), so a zero allowance refuses
/// every hosted call in the collection — the same hard opt-out
/// `admit_remote_execution` gives a single hosted dispatch.
///
/// **Budget-0 contract divergence, named (#1442 gate):** where CLAUDE.md's
/// `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION` doc promises "setting it to 0
/// refuses all remote calls with a typed error," `dispatch.single_shot`'s
/// hosted arm honors that as a step-level `Err` (`admit_remote_execution`);
/// `dispatch.map` instead completes `Ok` with EVERY item skipped and the
/// budget reason named per item. Coherent for a map — surviving partial
/// failure is its whole point, and an all-skipped result array is louder
/// per item than one opaque step error — but the two surfaces do differ,
/// and this sentence is where that difference is recorded.
///
/// **Allowance multiplication, for the ship-2b follow-on (#1442):** the
/// review pipeline today shares ONE stage bucket across every probe seat
/// (`Arc<Mutex<RemoteBucket>>` cloned into each `ReviewProbeStepKind`).
/// Restructuring probe onto seats x k sibling `dispatch.map` steps gives
/// each step its OWN full per-execution allowance — multiplying the
/// effective stage budget by the step count. The follow-on must resolve
/// that deliberately (a shared bucket-group mechanism, or an explicitly
/// accepted per-step reading), never inherit it silently.
struct MapRemoteBucket {
    budget: u64,
    used: u64,
    skipped: u32,
}

impl MapRemoteBucket {
    fn new(budget: u64) -> Self {
        Self { budget, used: 0, skipped: 0 }
    }
    fn exhausted(&self) -> bool {
        self.used >= self.budget
    }
    /// What is left to grant a single call (#1442 gate C6): per-item
    /// `max_tokens` clamps to THIS, not the full budget, so one late item
    /// cannot request more than the bucket has left.
    fn remaining(&self) -> u64 {
        self.budget.saturating_sub(self.used)
    }
    /// `false` ⇒ the bucket is exhausted and this item's call must not fire
    /// (counted as skipped, for the item's named-reason result).
    fn admit(&mut self) -> bool {
        if self.exhausted() {
            self.skipped += 1;
            false
        } else {
            true
        }
    }
    fn spend(&mut self, tokens: u64) {
        self.used = self.used.saturating_add(tokens);
    }
}

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
/// dialect), `n_ctx`/`identifier` (residency hints — see [`Self::residency`]).
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
            session_id: Some(format!("task:{}", step.task_id)),
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
    fn aggregate_record(
        step: &Step,
        model: &str,
        remote: bool,
        results: &[MapItemResult],
    ) -> darkmux_flow::FlowRecord {
        let ok_count = results.iter().filter(|r| r.ok).count();
        let failed_count = results.len() - ok_count;
        let total_tokens: u64 = results.iter().filter_map(|r| r.total_tokens).sum();
        darkmux_flow::FlowRecord {
            ts: darkmux_flow::ts_utc_now(),
            level: if failed_count == 0 { darkmux_flow::Level::Info } else { darkmux_flow::Level::Warn },
            category: darkmux_flow::Category::Work,
            tier: darkmux_flow::Tier::Local,
            stage: darkmux_flow::Stage::Dispatch,
            action: "step result".to_string(),
            handle: step.id.clone(),
            phase_id: None,
            session_id: Some(format!("task:{}", step.task_id)),
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
            })),
            work_id: None,
            attempt: None,
        }
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
        use crate::single_shot::{
            single_shot_chat, single_shot_chat_hosted, HostedSingleShotRequest, SingleShotRequest,
        };

        let items = resolve_map_collection(step, input)?;
        let mut flow_records = Vec::new();

        if items.is_empty() {
            // Empty-collection short-circuit (#1442): no dispatch, empty
            // output, a NAMED reason so the run's observability answers "why
            // did this map not dispatch" directly. `residency` already
            // returned `None` for this input, so no model was loaded. Runs
            // BEFORE the `model`/`user_template` requirements — a degenerate
            // upstream that produced nothing is a clean completed no-op, not
            // a config error (mirrors the review verify seat's empty-docket
            // short-circuit, which likewise never touches its dispatch config).
            flow_records.push(darkmux_flow::FlowRecord {
                ts: darkmux_flow::ts_utc_now(),
                level: darkmux_flow::Level::Info,
                category: darkmux_flow::Category::Work,
                tier: darkmux_flow::Tier::Local,
                stage: darkmux_flow::Stage::Dispatch,
                action: "step result".to_string(),
                handle: step.id.clone(),
                phase_id: None,
                session_id: Some(format!("task:{}", step.task_id)),
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
            });
            return Ok(StepOutcome { output: "[]".to_string(), flow_records });
        }

        let model = require_config_str(step, self.id(), "model")?;
        let user_template = require_config_str(step, self.id(), "user_template")?;
        let system = config_str(step, "system").unwrap_or("");
        let max_tokens = step.config.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(4096) as u32;
        let timeout_seconds =
            step.config.get("timeout_seconds").and_then(|v| v.as_u64()).unwrap_or(120) as u32;
        let endpoint: Option<darkmux_types::ModelEndpoint> = match step.config.get("endpoint") {
            Some(v) => Some(
                serde_json::from_value(v.clone())
                    .with_context(|| format!("step `{}`: config.endpoint", step.id))?,
            ),
            None => None,
        };

        // Per-EXECUTION remote allowance, shared across THIS step's whole
        // collection loop (the metered concept `DARKMUX_REMOTE_MAX_TOKENS_PER_
        // EXECUTION` governs — one execution = one pipeline stage/step). Local
        // items never draw from it.
        let budget = darkmux_types::config_access::remote_max_tokens_per_execution();
        let mut bucket = MapRemoteBucket::new(budget);

        let mut results: Vec<MapItemResult> = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let user = user_template.replace("{item}", &map_item_text(item));
            let res = if let Some(ep) = &endpoint {
                if !bucket.admit() {
                    MapItemResult {
                        index,
                        ok: false,
                        content: String::new(),
                        error: Some(
                            "remote token budget exhausted for this step — call skipped".to_string(),
                        ),
                        total_tokens: None,
                    }
                } else {
                    // (#1442 gate C6) Clamp to what the bucket has LEFT, not
                    // the full budget — a late item must not be granted more
                    // than the remaining allowance.
                    let clamped = clamp_hosted_max_tokens(max_tokens, bucket.remaining());
                    let req = HostedSingleShotRequest {
                        endpoint: ep,
                        model,
                        system,
                        user: &user,
                        max_tokens: clamped,
                        timeout_seconds,
                    };
                    match single_shot_chat_hosted(&req) {
                        Ok(reply) => {
                            bucket.spend(conservative_hosted_spend(reply.total_tokens, clamped));
                            MapItemResult {
                                index,
                                ok: true,
                                content: reply.content,
                                error: None,
                                total_tokens: reply.total_tokens,
                            }
                        }
                        // Per-item error ISOLATION: capture, continue.
                        Err(e) => MapItemResult {
                            index,
                            ok: false,
                            content: String::new(),
                            error: Some(format!("{e:#}")),
                            total_tokens: None,
                        },
                    }
                }
            } else {
                let temperature =
                    step.config.get("temperature").and_then(|v| v.as_f64()).unwrap_or(0.7) as f32;
                let req = SingleShotRequest {
                    base_url: None,
                    model,
                    system,
                    user: &user,
                    temperature,
                    max_tokens,
                    timeout_seconds,
                };
                match single_shot_chat(&req) {
                    Ok(reply) => MapItemResult {
                        index,
                        ok: true,
                        content: reply.content,
                        error: None,
                        total_tokens: reply.total_tokens,
                    },
                    // Per-item error ISOLATION: capture, continue.
                    Err(e) => MapItemResult {
                        index,
                        ok: false,
                        content: String::new(),
                        error: Some(format!("{e:#}")),
                        total_tokens: None,
                    },
                }
            };
            flow_records.push(Self::item_record(step, model, endpoint.is_some(), &res));
            results.push(res);
        }

        // (#1442 gate C1) ONE step-level aggregate record after the loop.
        // Load-bearing: the mission graph's token meter folds "step result"
        // token fields per step with Math.max (a one-record-per-step
        // assumption) — with only N per-item records, a map step's meter
        // would render as the LARGEST single item, not the step's spend. The
        // aggregate's SUMMED total_tokens is >= every per-item value, so the
        // existing max-fold reads the true spend with zero viewer changes.
        flow_records.push(Self::aggregate_record(step, model, endpoint.is_some(), &results));

        let output = serde_json::to_string(&results).context("serializing dispatch.map results")?;
        Ok(StepOutcome { output, flow_records })
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
        Some(darkmux_gestalt::Placement {
            model_key: model.to_string(),
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
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        }
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
            DispatchMapStepKind.residency(&s, &empty_task(), &BTreeMap::new()).is_none(),
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
        let placement = DispatchMapStepKind.residency(&s, &empty_task(), &BTreeMap::new()).unwrap();
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
        assert!(DispatchMapStepKind.residency(&s, &empty_task(), &BTreeMap::new()).is_none());
    }

    #[test]
    fn dispatch_map_residency_none_without_n_ctx_fails_open() {
        let s = map_step(json!({ "model": "m", "user_template": "u {item}", "collection": ["a"] }));
        assert!(
            DispatchMapStepKind.residency(&s, &empty_task(), &BTreeMap::new()).is_none(),
            "no n_ctx hint -> None (fail open to Remote scheduling), never a hard failure"
        );
    }

    #[test]
    fn map_remote_bucket_admits_until_exhausted_then_skips() {
        let mut b = MapRemoteBucket::new(100);
        assert!(b.admit(), "fresh bucket admits");
        b.spend(60);
        assert!(b.admit(), "still under budget");
        b.spend(60); // now 120 >= 100
        assert!(!b.admit(), "over budget -> skip");
        assert_eq!(b.skipped, 1);
    }

    #[test]
    fn map_remote_bucket_zero_budget_is_exhausted_from_the_first_item() {
        // The hard opt-out: a 0 allowance refuses every hosted call, the same
        // as `admit_remote_execution` refuses a single hosted dispatch.
        let mut b = MapRemoteBucket::new(0);
        assert!(!b.admit(), "zero budget admits nothing");
        assert!(b.exhausted());
    }

    #[test]
    fn map_remote_bucket_remaining_shrinks_with_spend_and_never_underflows() {
        // (#1442 gate C6) The per-item clamp target: what is LEFT, not the
        // full budget — a late item must not be granted more than remains.
        let mut b = MapRemoteBucket::new(100);
        assert_eq!(b.remaining(), 100);
        b.spend(70);
        assert_eq!(b.remaining(), 30, "a later item's grant clamps to 30, not 100");
        assert_eq!(
            clamp_hosted_max_tokens(4096, b.remaining()),
            30,
            "the clamp reads the remaining allowance"
        );
        b.spend(60); // overshoot: used 130 > budget 100
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

    #[test]
    fn dispatch_map_aggregate_record_sums_tokens_and_counts_outcomes() {
        // (#1442 gate C1) The one step-level aggregate: items_in, ok_count,
        // failed_count, remote, and SUMMED total_tokens — the record the
        // mission graph's max-fold token meter reads as the step's true
        // spend (any per-item value is <= the sum).
        let results = vec![
            MapItemResult { index: 0, ok: true, content: "a".to_string(), error: None, total_tokens: Some(100) },
            MapItemResult { index: 1, ok: false, content: String::new(), error: Some("boom".to_string()), total_tokens: None },
            MapItemResult { index: 2, ok: true, content: "c".to_string(), error: None, total_tokens: Some(250) },
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

        let clean = vec![MapItemResult { index: 0, ok: true, content: "a".to_string(), error: None, total_tokens: Some(5) }];
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
}
