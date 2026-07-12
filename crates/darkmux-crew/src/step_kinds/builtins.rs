//! Built-in step kinds (#1230 Packet 2): `dispatch.internal`,
//! `dispatch.single_shot`, `procedural.shell`, `procedural.noop`.
//!
//! Each kind reads its parameters from `Step.config` (a flat
//! `serde_json::Value` object — the same "kind-specific overflow bag"
//! pattern `WorkloadSpec.extras` and `ProfileModel.extras` already use).
//! Required keys are named in each kind's doc comment; a missing
//! required key is a loud `Err`, never a silent default that would mask
//! an operator/caller typo.

use super::types::{StepKind, StepOutcome};
use crate::types::Step;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;

/// Compose a step kind's base prompt/message with the gathered output of
/// its already-`Complete` dependencies. Shared by `dispatch.internal` and
/// `dispatch.single_shot` so the "prior step outputs" framing is
/// identical across both — mirrors the one-hop "Prior sprint outputs"
/// context block `dispatch::DispatchOpts::sprint_id` already builds for
/// Sprint-level `depends_on`, generalized to Step-level `depends_on`.
/// `input` iterates in `BTreeMap` key order (dependency step id), so the
/// composed text is deterministic regardless of completion order.
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

/// Wraps `dispatch::dispatch(DispatchOpts)` — a full agentic dispatch
/// through darkmux's internal Docker-bounded runtime. Required
/// `Step.config` keys: `role_id` (string), `message` (string, the base
/// prompt — prior-dependency output is prepended per `compose_message`).
/// Optional: `timeout_seconds` (u32, default 3600), `profile_name`
/// (string), `image` (string), `config_path` (string, `--profiles-file`
/// passthrough).
pub struct DispatchInternalStepKind;

impl StepKind for DispatchInternalStepKind {
    fn id(&self) -> &'static str {
        "dispatch.internal"
    }

    fn run(&self, step: &Step, input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        use crate::dispatch::{dispatch, CompactionDispatchArgs, DispatchOpts, Runtime};

        let role_id = require_config_str(step, self.id(), "role_id")?.to_string();
        let base_message = config_str(step, "message").unwrap_or_default();
        let message = compose_message(base_message, input);
        let timeout_seconds = step
            .config
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600) as u32;
        let profile_name = config_str(step, "profile_name").map(str::to_string);
        let image = config_str(step, "image").map(str::to_string);
        let config_path = config_str(step, "config_path").map(str::to_string);

        let opts = DispatchOpts {
            role_id,
            message,
            deliver: None,
            session_id: Some(format!("step:{}", step.id)),
            timeout_seconds,
            skip_preflight: false,
            json: true,
            watch_paths: Vec::new(),
            workdir: None,
            sprint_id: None,
            runtime: Runtime::Internal,
            runtime_cmd: "openclaw".to_string(),
            machine: None,
            wait: true,
            compaction: CompactionDispatchArgs::default(),
            profile_name,
            config_path,
            force_container: false,
            max_completion_tokens: None,
            image,
        };
        let result =
            dispatch(opts).with_context(|| format!("step `{}` dispatch.internal", step.id))?;
        Ok(StepOutcome {
            output: result.stdout,
            flow_records: Vec::new(),
        })
    }

    fn residency(&self, step: &Step) -> Option<darkmux_gestalt::Placement> {
        let role_id = config_str(step, "role_id")?;
        let profile_name = config_str(step, "profile_name");
        let config_path = config_str(step, "config_path");
        resolve_local_placement(role_id, profile_name, config_path, &format!("step:{}", step.id))
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
pub struct DispatchSingleShotStepKind;

impl StepKind for DispatchSingleShotStepKind {
    fn id(&self) -> &'static str {
        "dispatch.single_shot"
    }

    fn run(&self, step: &Step, input: &BTreeMap<String, String>) -> Result<StepOutcome> {
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

        let reply = if let Some(endpoint_val) = step.config.get("endpoint") {
            let endpoint: darkmux_types::ModelEndpoint = serde_json::from_value(endpoint_val.clone())
                .with_context(|| format!("step `{}`: config.endpoint", step.id))?;
            let req = HostedSingleShotRequest {
                endpoint: &endpoint,
                model,
                system,
                user: &user,
                max_tokens,
                timeout_seconds,
            };
            single_shot_chat_hosted(&req)
                .with_context(|| format!("step `{}` dispatch.single_shot (hosted)", step.id))?
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
            flow_records: Vec::new(),
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

    fn run(&self, step: &Step, input: &BTreeMap<String, String>) -> Result<StepOutcome> {
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

    fn run(&self, step: &Step, _input: &BTreeMap<String, String>) -> Result<StepOutcome> {
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
            depends_on: Vec::new(),
            status: crate::types::NodeStatus::Planned,
            config,
            started_ts: None,
            completed_ts: None,
            output: None,
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
    fn dispatch_internal_requires_role_id() {
        let s = step("s1", "dispatch.internal", json!({"message": "hi"}));
        let err = DispatchInternalStepKind.run(&s, &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("config.role_id"), "{err}");
    }

    #[test]
    fn dispatch_single_shot_requires_model() {
        let s = step("s1", "dispatch.single_shot", json!({"user": "hi"}));
        let err = DispatchSingleShotStepKind.run(&s, &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("config.model"), "{err}");
    }

    #[test]
    fn procedural_shell_requires_command() {
        let s = step("s1", "procedural.shell", json!({}));
        let err = ProceduralShellStepKind.run(&s, &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("config.command"), "{err}");
    }

    #[test]
    fn procedural_shell_runs_and_captures_stdout() {
        let s = step("s1", "procedural.shell", json!({"command": "echo hello-shell"}));
        let out = ProceduralShellStepKind.run(&s, &BTreeMap::new()).unwrap();
        assert!(out.output.contains("hello-shell"));
    }

    #[test]
    fn procedural_shell_nonzero_exit_is_an_error() {
        let s = step("s1", "procedural.shell", json!({"command": "exit 3"}));
        let err = ProceduralShellStepKind.run(&s, &BTreeMap::new()).unwrap_err();
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
        let out = ProceduralShellStepKind.run(&s, &input).unwrap();
        assert!(out.output.contains("value-from-upstream"), "got: {}", out.output);
    }

    #[test]
    fn procedural_noop_defaults_output_to_step_id() {
        let s = step("marker-step", "procedural.noop", json!(null));
        let out = ProceduralNoopStepKind.run(&s, &BTreeMap::new()).unwrap();
        assert_eq!(out.output, "marker-step");
    }

    #[test]
    fn procedural_noop_honors_config_output_override() {
        let s = step("s1", "procedural.noop", json!({"output": "custom"}));
        let out = ProceduralNoopStepKind.run(&s, &BTreeMap::new()).unwrap();
        assert_eq!(out.output, "custom");
    }
}
