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
use anyhow::{anyhow, Context, Result};
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

    fn residency(&self, step: &Step, task: &Task) -> Option<darkmux_gestalt::Placement> {
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
/// not once for the whole graph, matching how a bare `crew dispatch` (also
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
        assert_eq!(ProceduralShellStepKind.display_name(), "Shell");
        assert_eq!(ProceduralNoopStepKind.display_name(), "No-op");
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
