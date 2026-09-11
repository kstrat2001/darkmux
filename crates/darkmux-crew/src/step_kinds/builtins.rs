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
    CwdPolicy, MapDispatchOverride, OverrideDispatchCall, SeatClaim, StepKind, StepOutcome, StepRunCtx,
};
use super::MIN_VIABLE_MAP_GRANT;
use crate::remote_budget::RemoteBudget;
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
/// this with `dispatch_wire_model_id` / `compactor_wire_model_id` in
/// `dispatch_internal.rs` (which resolve against a `Profile`, not a `Step`)
/// is left to #2537; the shapes differ enough (a `Step.config` string vs. a
/// `Profile.models[]` lookup) that forcing one signature over both would
/// widen this fix rather than just closing it.
///
/// HOSTED steps (`config.endpoint` present) never call this: an endpoint
/// deployment name is not something darkmux loads into local residency, so
/// the bare `config.model` string is already the correct wire value there
/// — see each `seat()`'s own `RemoteEndpoint` short-circuit.
fn local_dispatch_wire_model_id(step: &Step, model: &str) -> String {
    config_str(step, "identifier")
        .map(str::to_string)
        .unwrap_or_else(|| darkmux_gestalt::namespaced_identifier(model, None))
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
/// - the winning model is ENDPOINT-BEARING → [`SeatClaim::RemoteEndpoint`],
///   which is correct and SILENT: it was never going to touch local
///   residency;
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
        Err(PlacementMiss::Remote) => SeatClaim::RemoteEndpoint,
        Err(PlacementMiss::ResolutionFailed(reason)) => SeatClaim::LocalModelUnresolved { reason },
    }
}

/// Why [`resolve_local_placement_inner`] returned no `Placement` — see
/// [`resolve_local_seat`]'s doc. `Remote` is the legitimate, silent case (an
/// endpoint-bearing model was never going to need local residency);
/// `ResolutionFailed` is the loud one.
enum PlacementMiss {
    Remote,
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
    // machine-local `role_profiles` map FIRST (`dispatch_internal::
    // resolve_role_aware_profile`, #1547) and `default_profile` only as the
    // fallback. This placement used to read `default_profile` alone, so a
    // bound role (`role_profiles.coder = coder-qwen38` on a registry whose
    // default is `balanced`) had its wave load and lease model A while every
    // dispatch loaded model B — lived on 2026-09-04: a five-coder wave leased
    // turboquant and each coder then loaded qwen3.8 itself. Same map, same
    // precedence, read live like the dispatch does (test builds see an empty
    // map by construction, #811 — the pure core below takes it as a value).
    let mapped = if profile_name.is_none() {
        darkmux_types::config_access::role_profile(role_id)
    } else {
        None
    };
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
    use crate::select::select_model;
    use PlacementMiss::ResolutionFailed;

    let loaded = darkmux_profiles::profiles::load_registry(config_path)
        .map_err(|e| ResolutionFailed(format!("profile registry: {e}")))?;
    let profile = match (profile_name, mapped) {
        // An explicit name wins (falling back to `default_profile` when this
        // machine does not define it — the machine-agnostic-caller case
        // `resolve_active` exists for).
        (Some(_), _) | (None, None) => {
            loaded
                .registry
                .resolve_active(profile_name)
                .ok_or_else(|| ResolutionFailed("no active profile".to_string()))?
                .1
        }
        // The `role_profiles` binding — loud when it names a profile the
        // registry does not define, never a silent fallback (contract 7),
        // exactly as the dispatch resolves it.
        (None, Some(mapped)) => {
            let binding = darkmux_profiles::profiles::RoleBinding::Mapped(mapped);
            darkmux_profiles::profiles::resolve_role_profile_with(role_id, &binding, &loaded.registry)
                .map_err(|e| ResolutionFailed(format!("{e:#}")))?
                .profile
        }
    };

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
) -> Result<crate::dispatch::DispatchOpts> {
    use crate::dispatch::{CompactionDispatchArgs, DispatchOpts};

    let role_id = task_or_config_str(task.role_id.as_ref(), step, "role_id").ok_or_else(|| {
        // The kind id is spelled out rather than read off `self.id()` — this
        // is a free function now, and the literal is the same constant that
        // method returns, so the message is byte-identical to before.
        anyhow!("step `{}`: `dispatch.internal` requires task.role_id or config.role_id", step.id)
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
    // The owning Task names the phase for every step minted from a mission
    // config; the step config's own `phase_id` (the crew-of-one's way of
    // passing the CLI's `--phase`) wins when present. Without the task
    // fallback a config-launched dispatch left with no phase and so no
    // mission on its records: no drill link from the mission view, no
    // events in the sheet, no token attribution (2026-09-04, the grown
    // follow-on steps of a crawl).
    let phase_id = config_str(step, "phase_id")
        .map(str::to_string)
        .or_else(|| (!task.phase_id.is_empty()).then(|| task.phase_id.clone()));
    let session_id = config_str(step, "session_id")
        .map(str::to_string)
        .unwrap_or_else(|| darkmux_types::session_id::step(&step.id));
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
    // (#2114 follow-up) `--resume-from <dir>` threaded through the
    // crew-of-one graph's step config (`DispatchAsCrewOfOne::build_graph`)
    // — see that fn's own doc for why the CLI's `DispatchOpts` isn't
    // forwarded wholesale.
    let resume_from = config_str(step, "resume_from").map(std::path::PathBuf::from);
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
    let brief_refs = crate::brief_refs::from_json(step.config.get("brief_refs"));
    let (message, brief_refs) = crate::brief_refs::append_to_brief(
        &message,
        &brief_refs,
        &crate::brief_refs::StoreDirs::resolved(),
    )
    .with_context(|| format!("step `{}`: resolving the brief's records", step.id))?;

    let opts = DispatchOpts {
        brief_refs,
        workspace_read_only: false,
        record_context: None,
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
        timeout_override_seconds: step
            .config
            .get("timeout_override_seconds")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok()),
    };
    Ok(opts)
}

impl StepKind for DispatchInternalStepKind {
    fn id(&self) -> &'static str {
        "dispatch.internal"
    }

    fn display_name(&self) -> &'static str {
        "Dispatch"
    }

    fn run(&self, step: &Step, task: &Task, input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        use crate::dispatch::dispatch;

        let parse_verifiers = step
            .config
            .get("parse_verifiers")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let preserve_dispatch_result = step
            .config
            .get("preserve_dispatch_result")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // (#2480 review, blocker 2) The whole `Step`/`Task` ->
        // `DispatchOpts` reconstruction lives in `dispatch_opts_for`, a
        // free function a unit test can call without Docker or a model —
        // so a field can no longer be dropped on this hop while the suite
        // stays green. Everything below is post-dispatch handling.
        let opts = dispatch_opts_for(step, task, input)?;
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
        let Some(role_id) = task_or_config_str(task.role_id.as_ref(), step, "role_id") else {
            return SeatClaim::LocalModelUnresolved {
                reason: "no role_id on the task or in step config".to_string(),
            };
        };
        let profile_name = task_or_config_str(task.profile_name.as_ref(), step, "profile_name");
        let config_path = config_str(step, "config_path").map(str::to_string);
        // NOTE: `step:{id}` here is a gestalt SEAT LABEL (placement-plan
        // diagnostics), NOT a flow-record session id — exempt from the #1436
        // hyphen convention; future colon sweeps should skip it.
        resolve_local_seat(&role_id, profile_name.as_deref(), config_path.as_deref(), &format!("step:{}", step.id))
    }

    /// (#1511) The role this kind dispatches, read from the SAME
    /// `task_or_config_str` source `run` and `seat` above both read — one
    /// expression, three callers, so the consent gate cannot disagree with
    /// the load. `None` only when neither the Task nor the config names a
    /// role, which is the same input `seat` reports as
    /// `LocalModelUnresolved` (no wave load) and `run` fails on.
    fn dispatch_role(
        &self,
        step: &Step,
        task: &Task,
        _input: &std::collections::BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> Option<String> {
        task_or_config_str(task.role_id.as_ref(), step, "role_id")
    }

    /// (#2614 review, MUST FIX + "Also fix" wrong-problem-surfaced finding)
    /// The scheduler-hoisted half of the `--resume-from` checkpoint gate —
    /// see `StepKind::resume_precheck`'s own doc for the full "why here,
    /// why not the workdir check too" reasoning. The early-out on a bare
    /// `config_str` read (no `resume_from` key at all) stays cheap and
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
        _ctx: &StepRunCtx,
    ) -> Result<()> {
        if config_str(step, "resume_from").is_none() {
            return Ok(());
        }
        let opts = dispatch_opts_for(step, task, input)
            .with_context(|| format!("step `{}` dispatch.internal", step.id))?;
        let Some(resume_from) = opts.resume_from.clone() else {
            return Ok(());
        };
        crate::dispatch_internal::refuse_resume_on_bare_hosted_path(&opts)
            .context("darkmux dispatch --resume-from")?;
        crate::dispatch_internal::validate_resume_checkpoint_content(&resume_from, &opts.role_id)
            .context("darkmux dispatch --resume-from")?;
        Ok(())
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
/// per-stage `crate::remote_budget::RemoteBudget` (#1877 promoted it into
/// this crate — it no longer lives only in `darkmux-lab::lab::review`),
/// which accumulates spend across many calls in one stage. The type being
/// reachable now does not make this kind's regime the same one: wiring
/// this kind onto a shared `RemoteBudget` bucket instead of a fresh
/// per-step allowance is a real behavior change, not a rename — that
/// consolidation is #1414's job. This PR closes the silent-bypass gap
/// (#1412); the shared-bucket regime is a deliberate follow-up, not a
/// scope cut hiding in this diff.
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
/// `null` for a field the endpoint never reported, never a fabricated `0`.
/// Nothing here derives one token field from another: whether reasoning
/// sits inside `completion_tokens` is provider-scoped (see the runtime
/// crate's `lmstudio::CompletionTokensDetails::reasoning_tokens`).
fn hosted_single_shot_step_payload(
    step_id: &str,
    budget: u64,
    max_tokens_requested: u32,
    max_tokens_sent: u32,
    reply: &crate::single_shot::SingleShotReply,
) -> serde_json::Value {
    serde_json::json!({
        "step_id": step_id,
        "kind": "dispatch.single_shot",
        "runtime": "direct",
        "remote_max_tokens_per_execution": budget,
        "max_tokens_requested": max_tokens_requested,
        "max_tokens_sent": max_tokens_sent,
        "prompt_tokens": reply.prompt_tokens,
        "completion_tokens": reply.completion_tokens,
        "total_tokens": reply.total_tokens,
        // (#1444, payload-additive — FLOW_SCHEMA_VERSION 1.44.0)
        "reasoning_tokens": reply.reasoning_tokens,
        "cached_tokens": reply.cached_tokens,
    })
}

impl DispatchSingleShotStepKind {
    /// (#2344) This kind's contract-#2 bookend records — the same shape
    /// `DispatchMapStepKind::bookend_record` builds for its own kind, and
    /// keyed on the SAME `session_id::task` this kind's `step result` record
    /// already uses, so a consumer joins the pair to the tokens.
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
            "kind": "dispatch.single_shot",
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
            session_id: Some(darkmux_types::session_id::task(&step.task_id)),
            source: Some("scheduler".to_string()),
            model: Some(model.to_string()),
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: Some(payload),
            work_id: None,
            attempt: None,
        }
    }
}

impl StepKind for DispatchSingleShotStepKind {
    fn id(&self) -> &'static str {
        "dispatch.single_shot"
    }

    fn display_name(&self) -> &'static str {
        "Dispatch (single-shot)"
    }

    /// (#1979) Task-scoped, NOT the trait default's step scope. Deliberate:
    /// sibling seats fanned out within one task share this key so a
    /// consumer can join a seat's tokens to its endpoint (see the record
    /// built in this kind's own dispatch path, and `session_id::task`'s
    /// doc). The step remains individually attributable through
    /// `payload.step_id` and `handle` — grouping and identity are different
    /// jobs, and this field is the grouping one.
    fn dispatch_session_id(&self, step: &Step) -> Option<String> {
        if let Some(sid) = step.config.get("session_id").and_then(|v| v.as_str()) {
            if !sid.is_empty() {
                return Some(sid.to_string());
            }
        }
        Some(darkmux_types::session_id::task(&step.task_id))
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
        if step.config.get("endpoint").is_some() {
            return SeatClaim::RemoteEndpoint;
        }
        let Some(model) = config_str(step, "model") else {
            return SeatClaim::LocalModelUnresolved { reason: "no config.model".to_string() };
        };
        let Some(min_ctx) = step.config.get("n_ctx").and_then(|v| v.as_u64()).and_then(|n| u32::try_from(n).ok())
        else {
            return SeatClaim::LocalModelUnresolved {
                reason: format!("local model `{model}` has no usable config.n_ctx"),
            };
        };
        let identifier = local_dispatch_wire_model_id(step, model);
        let model_key = config_str(step, "model_key").unwrap_or(model);
        SeatClaim::LocalModel(darkmux_gestalt::Placement {
            model_key: model_key.to_string(),
            identifier,
            min_ctx,
            seat: format!("step:{}", step.id),
        })
    }

    /// (#1511) `None` — this kind dispatches a bare MODEL, never a role.
    /// `run` below builds its request from `config.model` + `config.user` +
    /// `config.system`; it never resolves a role manifest and never loads a
    /// role prompt, and `seat` above reads `config.model`/`config.n_ctx`
    /// for the same reason. There is no role doctrine for the
    /// licensed-adjacent gate to disclose, so it has nothing to gate — the
    /// behavior this kind had before #1511 and still has.
    fn dispatch_role(
        &self,
        _step: &Step,
        _task: &Task,
        _input: &BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> Option<String> {
        None
    }

    /// The ctx-free entry point unit tests drive directly. Production takes
    /// [`Self::run_streaming`] below; both funnel into `run_single_shot`, so
    /// the presence beat and the liveness bookends are on ONE path, not two.
    fn run(&self, step: &Step, task: &Task, input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        self.run_single_shot(step, task, input, None)
    }

    /// (#2344) The scheduler's real entry point. Overridden for the same
    /// reason `dispatch.map` overrides it: the liveness bookends ride the
    /// STREAMING seam (see [`StepBookend`]), so a kind that only implements
    /// `run` can emit a start but never a live terminal.
    fn run_streaming(
        &self,
        step: &Step,
        task: &Task,
        input: &BTreeMap<String, String>,
        ctx: &StepRunCtx,
    ) -> Result<StepOutcome> {
        self.run_single_shot(step, task, input, Some(ctx))
    }
}

impl DispatchSingleShotStepKind {
    fn run_single_shot(
        &self,
        step: &Step,
        _task: &Task,
        input: &BTreeMap<String, String>,
        ctx: Option<&StepRunCtx>,
    ) -> Result<StepOutcome> {
        use crate::single_shot::{
            single_shot_chat, single_shot_chat_hosted, HostedSingleShotRequest,
            SingleShotRequest,
        };

        let model = require_config_str(step, self.id(), "model")?;
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
        let is_hosted = step.config.get("endpoint").is_some();
        let wire_model: std::borrow::Cow<'_, str> = if is_hosted {
            std::borrow::Cow::Borrowed(model)
        } else {
            std::borrow::Cow::Owned(local_dispatch_wire_model_id(step, model))
        };
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

        // (#2344) Contract #2's liveness bookends, which this kind owed and
        // never emitted — it performs REAL model work (one chat completion,
        // local or hosted) and emitted only its own `step result` vocabulary,
        // so a `dispatch.single_shot` seat had tokens and a model but no
        // start and no terminal anywhere. Strictly worse than `dispatch.map`,
        // which at least had the pair. Opened BEFORE any model work and
        // closed on every exit path, `?` and panic included, via
        // `StepBookend`'s Drop.
        let endpoint_label: Option<String> = step
            .config
            .get("endpoint")
            .and_then(|v| serde_json::from_value::<darkmux_types::ModelEndpoint>(v.clone()).ok())
            .map(|ep| crate::dispatch_internal::remote_endpoint_label(&ep, wire_model.as_ref()));
        let mut bookend = StepBookend::new(
            ctx,
            Self::bookend_record(
                step,
                wire_model.as_ref(),
                "dispatch start",
                darkmux_flow::Level::Info,
                endpoint_label.as_deref(),
                serde_json::json!({}),
            ),
            Self::bookend_record(
                step,
                wire_model.as_ref(),
                "dispatch error",
                darkmux_flow::Level::Error,
                endpoint_label.as_deref(),
                serde_json::json!({
                    "result_class": "error",
                    "error": "dispatch.single_shot terminated before completion (early return or panic)",
                }),
            ),
        );

        // (#2344) Session-liveness heartbeat — the same in-process twin of
        // the container path's emitter (#638) `dispatch.map` grew, opened at
        // the same point the bookends open and keyed on the SAME
        // `session_id::task` every record on this path uses. One hosted
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
        let mut session_emitter = darkmux_flow::session_presence::spawn_session_emitter(
            darkmux_types::session_id::task(&step.task_id),
            None,
            Some(wire_model.to_string()),
        );

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
                model: wire_model.as_ref(),
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
                model: Some(wire_model.to_string()),
                reasoning: None,
                mission_id: None,
                machine_id: None,
                machine_uid: None,
                prev_hash: None,
                hash: None,
                payload: Some(hosted_single_shot_step_payload(
                    &step.id,
                    budget,
                    max_tokens,
                    clamped_max_tokens,
                    &reply,
                )),
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
                model: wire_model.as_ref(),
                system,
                user: &user,
                temperature,
                max_tokens,
                timeout_seconds,
            };
            single_shot_chat(&req)
                .map_err(|e| with_residency_lost_hint(wire_model.as_ref(), e))
                .with_context(|| format!("step `{}` dispatch.single_shot (local)", step.id))?
        };

        // (#2344) The one call is over — no model work is in flight for this
        // step — so stop the heartbeat before the terminal record, the same
        // ordering `dispatch.map` and the container path both use.
        if let Some(em) = session_emitter.take() {
            em.stop();
        }
        bookend.close(Self::bookend_record(
            step,
            wire_model.as_ref(),
            "dispatch complete",
            darkmux_flow::Level::Info,
            endpoint_label.as_deref(),
            serde_json::json!({
                "result_class": "ok",
                "stdout_chars": reply.content.len(),
                "total_tokens": reply.total_tokens,
            }),
        ));

        Ok(StepOutcome {
            output: reply.content,
            flow_records,
        })
    }
}

// ─── dispatch.map (#1442) ───────────────────────────────────────────────

// (#1442) `RemoteBudget` (`crate::remote_budget`, #1877's shared home) lets
// the SCHEDULER own a `bucket_group -> Arc<Mutex<RemoteBudget>>` map and
// hand the same bucket to sibling `dispatch.map` steps (the "allowance
// multiplication" carry-forward — see that type's doc and `StepRunCtx`).
// The budget-0 divergence (a grouped-or-ungrouped `dispatch.map` completes
// `Ok` with every item skipped rather than a step-level `Err`, unlike
// `dispatch.single_shot`'s hosted arm) is unchanged and documented on `run`
// below.

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
    /// [`map_item_token_payload`] omits the key rather than inventing a
    /// zero. Tracked with flags INDEPENDENT of `any_split` — see
    /// [`accumulate_details`].
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
/// being hijacked; the shape is reserved and named in DESIGN.md's glossary.
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
/// per-item record shape — with a per-item loop and a step-scoped remote
/// bucket ([`RemoteBudget`]) added on top. That there is no genuinely-new
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
        task: &Task,
        input: &BTreeMap<String, String>,
        ctx: Option<&StepRunCtx>,
    ) -> Result<StepOutcome> {
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
            push(Self::short_circuit_record(step), &mut batched);
            return Ok(StepOutcome { output: "[]".to_string(), flow_records: batched });
        }

        let model = require_config_str(step, self.id(), "model")?;
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
        let is_hosted = step.config.get("endpoint").is_some();
        let wire_model: std::borrow::Cow<'_, str> = if is_hosted {
            std::borrow::Cow::Borrowed(model)
        } else {
            std::borrow::Cow::Owned(local_dispatch_wire_model_id(step, model))
        };
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
        // `StepBookend`'s Drop emits `dispatch error` unless `close` already
        // consumed the terminal. This is what gives a per-seat `task-<id>`
        // session an endpoint to be attributed by — without it the seat's
        // token records name a model and a cost but never a place.
        let endpoint_label: Option<String> = endpoint
            .as_ref()
            .map(|ep| crate::dispatch_internal::remote_endpoint_label(ep, wire_model.as_ref()));
        let mut bookend = StepBookend::new(
            ctx,
            Self::bookend_record(
                step,
                wire_model.as_ref(),
                "dispatch start",
                darkmux_flow::Level::Info,
                endpoint_label.as_deref(),
                serde_json::json!({ "items_in": items.len() }),
            ),
            Self::bookend_record(
                step,
                wire_model.as_ref(),
                "dispatch error",
                darkmux_flow::Level::Error,
                endpoint_label.as_deref(),
                serde_json::json!({
                    "result_class": "error",
                    "error": "dispatch.map terminated before completion (early return or panic)",
                }),
            ),
        );

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
        // every record on this path uses (`session_id::task`), so a beat and
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
        // `session_id::task` deliberately (sibling seats fanned out within
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
            darkmux_types::session_id::task(&step.task_id),
            task.role_id.clone(),
            Some(wire_model.to_string()),
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
        let bucket: Arc<Mutex<RemoteBudget>> = match ctx.and_then(|c| c.remote_bucket()) {
            Some(shared) => shared.clone(),
            None => {
                let budget = step
                    .config
                    .get("bucket_budget")
                    .and_then(|v| v.as_u64())
                    .unwrap_or_else(darkmux_types::config_access::remote_max_tokens_per_execution);
                Arc::new(Mutex::new(RemoteBudget::new(budget, MIN_VIABLE_MAP_GRANT)))
            }
        };
        // (#1442 ship-2b) The scheduler-supplied dispatch override, if any —
        // threaded into every item's arm; `None` on all production paths.
        let ovr = ctx.and_then(|c| c.dispatch_override());

        let temperature =
            step.config.get("temperature").and_then(|v| v.as_f64()).unwrap_or(0.7) as f32;
        let mut results: Vec<MapItemResult> = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            // (#2310 P1 review finding I1) A `{system, item}` override wins
            // for THIS item's dispatch; every other item shape keeps using
            // the step's own `config.system` unchanged — see
            // `item_system_and_payload`'s own doc.
            let (item_system_override, payload) = item_system_and_payload(item);
            let item_system = item_system_override.unwrap_or(system);
            let user = user_template.replace("{item}", &map_item_text(payload));
            let res = match &endpoint {
                Some(ep) => map_hosted_item(
                    index, &bucket, ep, wire_model.as_ref(), item_system, &user, max_tokens,
                    timeout_seconds, retry_on_empty, retry_on_error, ovr,
                ),
                None => map_local_item(
                    index, wire_model.as_ref(), item_system, &user, temperature, max_tokens,
                    timeout_seconds, retry_on_empty, retry_on_error, ovr,
                ),
            };
            // (#1442 gate C3) LIVE per-item emission when streaming.
            push(Self::item_record(step, wire_model.as_ref(), endpoint.is_some(), &res), &mut batched);
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
                        Some(wire_model.as_ref()),
                        None,
                        None,
                        payload,
                    ),
                    &mut batched,
                );
            }
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
        push(Self::aggregate_record(step, wire_model.as_ref(), endpoint.is_some(), &results), &mut batched);

        // (#1607) The clean terminal. `remote_tokens` is stamped only for a
        // hosted seat, and only the SUM the seat actually spent — the same
        // figure the aggregate reports, so the two never disagree.
        let ok_count = results.iter().filter(|r| r.ok).count();
        let spent: u64 = results.iter().filter_map(|r| r.total_tokens).sum();
        let mut done = Self::bookend_record(
            step,
            wire_model.as_ref(),
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
}

impl Drop for StepBookend<'_> {
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
    // (#1444 review) Same omit-never-zero rule as the split above. NOTE the
    // deliberate divergence from the runtime-side `telemetry.tokens`
    // producers (`turn_tokens_payload`, `dispatch_remote`), which emit these
    // keys as explicit JSON `null` when unreported: this emitter's whole
    // convention is omission, and mixing the two inside one payload would be
    // worse than either. Both readings mean "the provider didn't say"; the
    // family-wide reconciliation this function's own doc already flags for
    // `review_token_telemetry_payload` covers these two as well.
    if let Some(r) = res.reasoning_tokens {
        obj.insert("reasoning_tokens".into(), serde_json::json!(r));
    }
    if let Some(c) = res.cached_tokens {
        obj.insert("cached_tokens".into(), serde_json::json!(c));
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

/// (#1444 review) The `reasoning_tokens`/`cached_tokens` sibling of
/// [`accumulate_split`], with the same honesty rule and one difference that
/// matters: the two fields carry INDEPENDENT "was it ever reported" flags
/// rather than sharing one.
///
/// `accumulate_split` can share `any_split` because a provider that reports
/// a usage split reports both halves of it. These two are genuinely
/// independent — `completion_tokens_details` and `prompt_tokens_details` are
/// separate objects, and a provider can name one without the other (the
/// runtime's own accumulator test pins exactly that turn shape: reasoning
/// reported, `prompt_tokens_details` absent). Folding them under a shared
/// flag would fabricate a `0` for whichever one the provider never named —
/// the absent-vs-zero collapse #1444 exists to prevent, reintroduced inside
/// the fix for it.
fn accumulate_details(
    reply_reasoning: Option<u64>,
    reply_cached: Option<u64>,
    rsum: &mut u64,
    cachesum: &mut u64,
    any_reasoning: &mut bool,
    any_cached: &mut bool,
) {
    if let Some(r) = reply_reasoning {
        *rsum += r;
        *any_reasoning = true;
    }
    if let Some(c) = reply_cached {
        *cachesum += c;
        *any_cached = true;
    }
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
    // (#1444 review) Same shape again for the usage DETAILS, with one flag
    // per field — see [`accumulate_details`] for why they can't share one.
    let (mut rsum, mut cachesum, mut any_reasoning, mut any_cached) = (0u64, 0u64, false, false);
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
                accumulate_details(
                    reply.reasoning_tokens,
                    reply.cached_tokens,
                    &mut rsum,
                    &mut cachesum,
                    &mut any_reasoning,
                    &mut any_cached,
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
                        reasoning_tokens: item_split_tokens(any_reasoning, rsum),
                        cached_tokens: item_split_tokens(any_cached, cachesum),
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
                        total_tokens: item_total_tokens(any_usage, sum),
                        prompt_tokens: item_split_tokens(any_split, psum),
                        completion_tokens: item_split_tokens(any_split, csum),
                        reasoning_tokens: item_split_tokens(any_reasoning, rsum),
                        cached_tokens: item_split_tokens(any_cached, cachesum),
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
        reasoning_tokens: item_split_tokens(any_reasoning, rsum),
        cached_tokens: item_split_tokens(any_cached, cachesum),
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
    bucket: &Arc<Mutex<RemoteBudget>>,
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
    // (#1444 review) Same shape again for the usage DETAILS, with one flag
    // per field — see [`accumulate_details`] for why they can't share one.
    let (mut rsum, mut cachesum, mut any_reasoning, mut any_cached) = (0u64, 0u64, false, false);
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
    // (#1605 QA finding) The error that sent us into a retry, kept so the
    // fallthrough below cannot report success for an item whose only real
    // dispatch FAILED. Before the retry arm existed, `Err` returned
    // immediately and the fallthrough's `ok: true` was sound — the only way
    // to reach it was a dispatched-but-empty reply. Retrying widened the set
    // of states that reach it without re-deriving that honesty.
    let mut last_error: Option<String> = None;
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
                    // No call fired, so there is no split — and (#1444
                    // review) no details — to report either.
                    prompt_tokens: None,
                    completion_tokens: None,
                    reasoning_tokens: None,
                    cached_tokens: None,
                    served_model: None,
                    wall_ms,
                    retried: 0,
                };
            }
            // A retry the bucket can no longer fund — stop, keep what fired.
            // (#1605 QA finding) Reachable ONLY on a retry, which means the
            // first attempt errored: sibling steps sharing this bucket_group
            // can drain it during the backoff window. Falling through to the
            // `ok: true` tail here would report a clean empty draw for an
            // item that only ever failed — and downstream that is counted as
            // a fired draw, suppressing both the "dispatch failed" warning
            // and the all-draws-failed gate this very PR hardens.
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
                    .settle(clamped, conservative_hosted_spend(reply.total_tokens, clamped), 1);
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
                accumulate_details(
                    reply.reasoning_tokens,
                    reply.cached_tokens,
                    &mut rsum,
                    &mut cachesum,
                    &mut any_reasoning,
                    &mut any_cached,
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
                        reasoning_tokens: item_split_tokens(any_reasoning, rsum),
                        cached_tokens: item_split_tokens(any_cached, cachesum),
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
                bucket.lock().expect("map remote bucket mutex poisoned").settle(clamped, 0, 1);
                if error_budget == 0 {
                    return MapItemResult {
                        index,
                        ok: false,
                        content: String::new(),
                        error: Some(format!("{e:#}")),
                        total_tokens: item_total_tokens(any_usage, sum),
                        prompt_tokens: item_split_tokens(any_split, psum),
                        completion_tokens: item_split_tokens(any_split, csum),
                        reasoning_tokens: item_split_tokens(any_reasoning, rsum),
                        cached_tokens: item_split_tokens(any_cached, cachesum),
                        served_model,
                        wall_ms,
                        retried: error_retries_used,
                    };
                }
                error_budget -= 1;
                error_retries_used += 1;
                last_error = Some(format!("{e:#}"));
                std::thread::sleep(RETRY_ON_ERROR_BACKOFF);
            }
        }
        attempt += 1;
    }
    // (#1605 QA finding) `ok` is a claim about what actually happened. An
    // item that reaches here after an errored attempt (the bucket-starved
    // retry path) must report that error, not an empty success.
    MapItemResult {
        index,
        ok: last_error.is_none(),
        content: String::new(),
        error: last_error,
        total_tokens: item_total_tokens(any_usage, sum),
        prompt_tokens: item_split_tokens(any_split, psum),
        completion_tokens: item_split_tokens(any_split, csum),
        reasoning_tokens: item_split_tokens(any_reasoning, rsum),
        cached_tokens: item_split_tokens(any_cached, cachesum),
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

    /// (#1979) Task-scoped, NOT the trait default's step scope. Deliberate:
    /// sibling seats fanned out within one task share this key so a
    /// consumer can join a seat's tokens to its endpoint (see the record
    /// built in this kind's own dispatch path, and `session_id::task`'s
    /// doc). The step remains individually attributable through
    /// `payload.step_id` and `handle` — grouping and identity are different
    /// jobs, and this field is the grouping one.
    fn dispatch_session_id(&self, step: &Step) -> Option<String> {
        if let Some(sid) = step.config.get("session_id").and_then(|v| v.as_str()) {
            if !sid.is_empty() {
                return Some(sid.to_string());
            }
        }
        Some(darkmux_types::session_id::task(&step.task_id))
    }


    fn run(&self, step: &Step, task: &Task, input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        // Ctx-free path (unit tests / callers with no scheduler seam): every
        // record batches into `StepOutcome.flow_records`, and the remote
        // bucket is step-scoped (no `bucket_group` sharing without a
        // scheduler to own the group map).
        self.run_map(step, task, input, None)
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
        task: &Task,
        input: &BTreeMap<String, String>,
        ctx: &StepRunCtx,
    ) -> Result<StepOutcome> {
        self.run_map(step, task, input, Some(ctx))
    }

    /// (#1442, restated as a seat claim by #2394) Four genuinely different
    /// answers this kind can give, which the old `Option<Placement>` had to
    /// squeeze into one `None`:
    ///
    /// - `endpoint` present → [`SeatClaim::RemoteEndpoint`]. Nothing local
    ///   to load; the hosted cap is exactly the right bound for it.
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
        if step.config.get("endpoint").is_some() {
            return SeatClaim::RemoteEndpoint;
        }
        match resolve_map_collection(step, task, input) {
            Ok(items) if items.is_empty() => return SeatClaim::NoModel,
            Ok(_) => {}
            Err(e) => {
                return SeatClaim::LocalModelUnresolved { reason: format!("collection: {e:#}") }
            }
        }
        let Some(model) = config_str(step, "model") else {
            return SeatClaim::LocalModelUnresolved { reason: "no config.model".to_string() };
        };
        let Some(min_ctx) = step.config.get("n_ctx").and_then(|v| v.as_u64()).and_then(|n| u32::try_from(n).ok())
        else {
            return SeatClaim::LocalModelUnresolved { reason: "no usable config.n_ctx".to_string() };
        };
        let identifier = local_dispatch_wire_model_id(step, model);
        // (#1442 ship-2b) `model_key` — the LOADABLE model key when it
        // differs from the wire `model` id. A local seat dispatches against
        // its darkmux-NAMESPACED identifier (`darkmux:<id>` as the wire
        // `model`), but the wave loader's `lms load` needs the bare model
        // key; without this override the loader would try to load the
        // namespaced string as if it were a model key.
        let model_key = config_str(step, "model_key").unwrap_or(model);
        SeatClaim::LocalModel(darkmux_gestalt::Placement {
            model_key: model_key.to_string(),
            identifier,
            min_ctx,
            // (#1442 gate C7) "step:<id>", consistent with the placement
            // provenance `dispatch.internal`'s seat claim uses.
            seat: format!("step:{}", step.id),
        })
    }

    /// (#1511) `None`, for the same reason `dispatch.single_shot` returns
    /// `None`: every item of the map is a bare model call built from
    /// `config.model`/`config.user`, with no role manifest anywhere. This
    /// is also what keeps an EMPTY `dispatch.map` — which claims
    /// [`SeatClaim::NoModel`] above and loads nothing — from being refused
    /// for a role it was never going to dispatch.
    fn dispatch_role(
        &self,
        _step: &Step,
        _task: &Task,
        _input: &BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> Option<String> {
        None
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
/// .or_else(|| config_str(step, "workdir"))` — Task first — and
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
fn resolve_shell_cwd(step: &Step, task: &Task) -> Result<Option<std::path::PathBuf>> {
    if let Some(explicit) = config_str(step, "cwd") {
        return Ok(Some(validated_shell_cwd(step, "step config `cwd`", std::path::Path::new(explicit))?));
    }
    if let Some(path) = task.workdir.as_deref() {
        return Ok(Some(validated_shell_cwd(step, "the owning task's `workdir`", path)?));
    }
    if let Some(explicit) = config_str(step, "workdir") {
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
    /// (#2394) [`SeatClaim::NoModel`] — this kind runs an operator-supplied shell command and
    /// speaks to no model at all. Before this hook it said nothing, and
    /// silence classified it as a hosted-endpoint dispatch: a wave of these
    /// queued one at a time behind `remote.concurrent_cap`, which a mission
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

    /// (#1511) `None` — the documented no-dispatch opt-out, matching this
    /// kind's [`SeatClaim::NoModel`] above. A shell command speaks to no
    /// model, so there is no role for the licensed-adjacent consent gate to
    /// check. This arm is why the gate's `None` case is not a fail-open:
    /// an ordinary `procedural.*` graph legitimately carries no role, and
    /// says so here rather than leaving the scheduler to guess.
    fn dispatch_role(
        &self,
        _step: &Step,
        _task: &Task,
        _input: &BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> Option<String> {
        None
    }

    fn id(&self) -> &'static str {
        "procedural.shell"
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
    fn dispatch_session_id(&self, _step: &Step) -> Option<String> {
        None
    }

    /// (#2577) The one kind that legitimately does — see
    /// [`CwdPolicy::AmbientWithRefusal`]'s own doc, and `resolve_shell_cwd`
    /// below for the resolution chain and the refusal it produces when the
    /// ambient directory has vanished.
    fn cwd_policy(&self) -> CwdPolicy {
        CwdPolicy::AmbientWithRefusal
    }

    fn run(&self, step: &Step, task: &Task, input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let command = require_config_str(step, self.id(), "command")?;
        let cwd = resolve_shell_cwd(step, task)?;

        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        if let Some(cwd) = &cwd {
            cmd.current_dir(cwd);
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
    /// queued one at a time behind `remote.concurrent_cap`, which a mission
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

    /// (#1511) `None` — the documented no-dispatch opt-out, matching this
    /// kind's [`SeatClaim::NoModel`] above. See
    /// `ProceduralShellStepKind::dispatch_role`.
    fn dispatch_role(
        &self,
        _step: &Step,
        _task: &Task,
        _input: &BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> Option<String> {
        None
    }

    fn id(&self) -> &'static str {
        "procedural.noop"
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
    fn dispatch_session_id(&self, _step: &Step) -> Option<String> {
        None
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

    /// (#1530 Packet 3a) A bare `StepRunCtx` — no emitter/bucket/override,
    /// an empty `ArtifactBus` — for tests that call `seat()` directly
    /// (bypassing the scheduler, which is the only production caller that
    /// materializes a real bus). None of `seat()`'s Tier 1 builtin
    /// implementations read the bus, so an empty one is sufficient here.
    fn bare_ctx() -> StepRunCtx {
        StepRunCtx::new(None, None, None, std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()))
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
        assert!(msg.contains("darkmux dispatch --resume-from"), "{msg}");
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
    /// `dispatch_remote_refuses_resume_from_before_the_http_call` in
    /// `dispatch_internal_tests.rs` uses) pinned at a `config_path`
    /// profiles registry whose only profile targets a REMOTE endpoint —
    /// no HTTP mock needed, since a correct refusal never dials it.
    #[test]
    #[serial_test::serial]
    fn dispatch_internal_resume_precheck_names_the_remote_single_shot_path_not_the_checkpoint() {
        let home = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", home.path()) };

        let registry_dir = TempDir::new().unwrap();
        let pf = registry_dir.path().join("profiles.json");
        std::fs::write(
            &pf,
            r#"{"profiles":{"cloud":{"models":[
                    {"id":"gpt-remote","n_ctx":100000,
                     "endpoint":{"url":"http://127.0.0.1:1"}}
                ]}},
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
            msg.contains("not supported on the remote single-shot dispatch path"),
            "must name the remote single-shot path as the reason: {msg}"
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
            local_dispatch_wire_model_id(&single, "qwen3-4b"),
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
        assert_eq!(local_dispatch_wire_model_id(&map, "qwen3-4b"), placement.identifier);
        assert_eq!(placement.identifier, "darkmux:qwen3-4b");
    }

    #[test]
    fn local_dispatch_wire_model_id_honors_an_explicit_identifier_override() {
        let s = step(
            "s1",
            "dispatch.single_shot",
            json!({ "model": "qwen3-4b", "identifier": "my-own-alias", "user": "hi", "n_ctx": 8192 }),
        );
        assert_eq!(local_dispatch_wire_model_id(&s, "qwen3-4b"), "my-own-alias");
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
    /// own `config_str(step, "identifier")` read in `local_dispatch_wire_
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
        assert_eq!(local_dispatch_wire_model_id(&m, "qwen3-4b"), "my-own-alias");
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
        let out = DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new());

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
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new());

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
            .filter(|r| r.action == "step result" && r.payload.as_ref().and_then(|p| p.get("index")).is_some())
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
            .find(|r| r.action == "step result" && r.payload.as_ref().and_then(|p| p.get("items_in")).is_some())
            .expect("aggregate_record must be present");
        assert_eq!(
            aggregate.model.as_deref(),
            Some("darkmux:qwen3-4b"),
            "aggregate_record must carry the namespaced identifier: {aggregate:?}"
        );

        let telemetry_records: Vec<&darkmux_flow::FlowRecord> =
            out.flow_records.iter().filter(|r| r.action == "telemetry.tokens").collect();
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
        // already uses. Reverting either `Self::bookend_record(step,
        // wire_model.as_ref(), ...)` call back to the bare `model` leaves
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
        let ctx = StepRunCtx::new(
            Some(tx),
            None,
            None,
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );
        let result = DispatchSingleShotStepKind.run_streaming(&s, &empty_task(), &BTreeMap::new(), &ctx);
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
            emitted.iter().filter(|r| r.action.starts_with("dispatch ")).collect();
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
        let out = DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new());

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
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new());

        unsafe {
            match prev {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }

        let out = out.expect("dispatch.map itself completes Ok even when an item fails");
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
        // SEPARATE construction site (line ~994, `Self::bookend_record(step,
        // wire_model.as_ref(), "dispatch error", ...)`), and nothing
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
        let ctx = StepRunCtx::new(
            Some(tx),
            None,
            None,
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );
        let result = DispatchSingleShotStepKind.run_streaming(&s, &empty_task(), &BTreeMap::new(), &ctx);
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
            emitted.iter().filter(|r| r.action == "dispatch error").collect();
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
        // panic through the SAME seam a real `bucket_group` sibling failure
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
        let ctx = StepRunCtx::new(
            Some(tx),
            None,
            Some(ovr),
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            DispatchMapStepKind.run_streaming(&s, &empty_task(), &BTreeMap::new(), &ctx)
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
            emitted.iter().filter(|r| r.action == "dispatch error").collect();
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
    // (#2532) `#[serial_test::serial]`: this step names no `cwd`/`workdir`,
    // so it READS the process cwd through `resolve_shell_cwd`'s ambient tier.
    // See `CwdGuard`'s doc — a reader participates in that global too.
    #[serial_test::serial]
    fn procedural_shell_runs_and_captures_stdout() {
        let s = step("s1", "procedural.shell", json!({"command": "echo hello-shell"}));
        let out = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        assert!(out.output.contains("hello-shell"));
    }

    #[test]
    // (#2532) `#[serial_test::serial]`: this step names no `cwd`/`workdir`,
    // so it READS the process cwd through `resolve_shell_cwd`'s ambient tier.
    // See `CwdGuard`'s doc — a reader participates in that global too.
    #[serial_test::serial]
    fn procedural_shell_nonzero_exit_is_an_error() {
        let s = step("s1", "procedural.shell", json!({"command": "exit 3"}));
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
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
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
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
        let out = ProceduralShellStepKind.run(&s, &empty_task(), &input).unwrap();
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
        let out = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
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
    /// `scheduler::tests::dispatch_free_siblings_do_not_serialize_behind_the_remote_cap`
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
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
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
        let out = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
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
        let out = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        assert_eq!(
            std::fs::canonicalize(out.output.trim()).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
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
        let out = ProceduralShellStepKind.run(&s, &task, &BTreeMap::new()).unwrap();
        assert_eq!(
            std::fs::canonicalize(out.output.trim()).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    /// (#2532) **The tier order between the two spellings of the SHARED
    /// `workdir` key, pinned.** `workdir` means one thing across kinds:
    /// `dispatch_opts_for` resolves it `task.workdir.clone().or_else(||
    /// config_str(step, "workdir"))` — Task first — and `StepKind`'s own
    /// doc states the same contract. Before this test, hoisting either
    /// branch above the other built clean and left the whole crate green,
    /// so nothing pinned it in either direction. Red-proved by swapping the
    /// `task.workdir` and `config_str(step, "workdir")` branches in
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
        let out = ProceduralShellStepKind.run(&s, &task, &BTreeMap::new()).unwrap();
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
        let out = ProceduralShellStepKind.run(&s, &task, &BTreeMap::new()).unwrap();
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
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
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
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
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
        let err = ProceduralShellStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("is set but empty"), "{rendered}");
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
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new()).unwrap();
        assert_eq!(out.output, "[]");
        assert_eq!(out.flow_records.len(), 1, "one short-circuit record");
        let payload = out.flow_records[0].payload.as_ref().unwrap();
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
    fn dispatch_map_hosted_claims_a_remote_endpoint() {
        // An endpoint-bearing (remote) map loads nothing locally — and says
        // REMOTE, which is what `remote.concurrent_cap` is for.
        let s = map_step(json!({
            "model": "gpt-5.1",
            "user_template": "check {item}",
            "n_ctx": 8192,
            "collection": ["a"],
            "endpoint": { "url": "https://example.com" },
        }));
        assert!(matches!(
            DispatchMapStepKind.seat(&s, &empty_task(), &BTreeMap::new(), &bare_ctx()),
            SeatClaim::RemoteEndpoint
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

    #[test]
    fn map_remote_bucket_admits_until_exhausted_then_skips() {
        // (#1617 review) Magnitudes scaled x1000 from the original 100/60.
        // Those were arbitrary small numbers chosen to exercise the ARITHMETIC,
        // but they sit below `MIN_VIABLE_MAP_GRANT` — so once the starvation
        // floor landed, this test was measuring the floor instead of the
        // accounting it exists to pin. Same shape, realistic token counts.
        let mut b = RemoteBudget::new(100_000, MIN_VIABLE_MAP_GRANT);
        let g1 = b.admit_reserve(60_000).expect("fresh bucket admits");
        b.settle(g1, 60_000, 1);
        let g2 = b.admit_reserve(60_000).expect("still under budget");
        b.settle(g2, 60_000, 1); // now 120k >= 100k (the endpoint reported above its grant)
        assert!(b.admit_reserve(60_000).is_none(), "over budget -> skip");
        assert_eq!(b.skipped(), 1);
    }

    #[test]
    fn a_bucket_starved_retry_reports_the_error_not_a_fabricated_empty_success() {
        // (#1605 QA finding) The regression the retry arm introduced. Before
        // `retry_on_error`, an `Err` returned immediately, so the loop's
        // fallthrough could only be reached after a dispatched-but-EMPTY
        // reply — `ok: true, error: None` was honest there. Retrying widened
        // the reachable set without re-deriving that: if the bucket is
        // drained during the 200ms backoff (siblings sharing a `bucket_group`
        // run concurrently — exactly the near-exhaustion regime), the retry's
        // admit is refused, the loop breaks, and the item fell through
        // claiming a clean empty draw for a dispatch that only ever FAILED.
        //
        // Downstream that is counted as a fired draw: no "dispatch failed"
        // warning, and the all-draws-failed gate — the very thing #1605
        // hardens — is suppressed. An error laundered into reduced coverage,
        // on the path built to stop exactly that.
        //
        // The race is driven deterministically: the override errors on the
        // first call and drains the shared bucket from inside that call, so
        // by the time the retry asks to be admitted there is nothing left.
        let budget = 10_000u64;
        let bucket = Arc::new(Mutex::new(RemoteBudget::new(budget, MIN_VIABLE_MAP_GRANT)));
        let drain = Arc::clone(&bucket);
        let calls = Arc::new(Mutex::new(0usize));
        let seen = Arc::clone(&calls);
        let ovr: MapDispatchOverride = Arc::new(move |_call: &OverrideDispatchCall<'_>| {
            *seen.lock().unwrap() += 1;
            // Consume the rest of the allowance mid-call, the way a
            // concurrent sibling step would.
            if let Ok(mut b) = drain.lock() {
                if let Some(g) = b.admit_reserve(budget as u32) {
                    b.settle(g, budget, 1);
                }
            }
            anyhow::bail!("endpoint refused the draw")
        });
        let endpoint = darkmux_types::ModelEndpoint {
            url: Some("http://127.0.0.1:1".to_string()),
            ..Default::default()
        };

        let out = map_hosted_item(
            0, &bucket, &endpoint, "gpt-5.1", "sys", "user", 1_000, 1, 0, 1, Some(&ovr),
        );

        assert!(
            !out.ok,
            "an item whose only dispatch errored must never report ok: {out:?}"
        );
        assert!(
            out.error.is_some(),
            "the error must survive the bucket-starved retry, not be dropped: {out:?}"
        );
        assert!(
            out.error.as_deref().unwrap_or_default().contains("endpoint refused the draw"),
            "and it must be the REAL error, not a generic budget message: {out:?}"
        );
        assert_eq!(*calls.lock().unwrap(), 1, "the retry was starved, so only one call fired");
    }

    #[test]
    fn map_remote_bucket_reservation_holds_the_grant_until_settled() {
        // (#1442 fan-out) The reserve-then-settle shape: a granted call's cap
        // is held against the budget WHILE the call is in flight, so a
        // concurrent sibling admitting mid-flight sees the reservation —
        // never the untouched balance (the allowance-multiplication race a
        // spend-after pair reintroduces under `seats x k` sibling
        // concurrency).
        let mut b = RemoteBudget::new(100, MIN_VIABLE_MAP_GRANT);
        let granted = b.admit_reserve(4096).expect("admits");
        assert_eq!(granted, 100, "the grant clamps to what remains");
        assert!(b.admit_reserve(10).is_none(), "an in-flight reservation blocks siblings");
        assert_eq!(b.skipped(), 1);
        // Settling with the real (higher) usage keeps the overshoot honest…
        b.settle(granted, 600, 1);
        assert!(b.exhausted());
        // …and settling an ERRORED call with 0 releases the whole grant.
        let mut b2 = RemoteBudget::new(100, MIN_VIABLE_MAP_GRANT);
        let g = b2.admit_reserve(4096).expect("admits");
        b2.settle(g, 0, 1);
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
        let mut b = RemoteBudget::new(100_000, MIN_VIABLE_MAP_GRANT);
        let g = b.admit_reserve(99_960).expect("the first draw admits");
        b.settle(g, 99_960, 1);
        assert_eq!(b.remaining(), 40);
        assert!(
            b.admit_reserve(4096).is_none(),
            "a 40-token grant cannot hold a reply — deny it rather than truncate"
        );
        assert_eq!(b.skipped(), 1, "and COUNT it, or the run reports coverage it never had");

        // Not starvation: the operator configured a tiny BUDGET. That is
        // policy — the only documented refusal value for the knob is 0, so a
        // floor that swallowed small budgets would invent a second one.
        let mut tiny = RemoteBudget::new(100, MIN_VIABLE_MAP_GRANT);
        assert_eq!(
            tiny.admit_reserve(4096),
            Some(100),
            "a deliberately small budget is operator policy, not a starved grant"
        );
        assert_eq!(tiny.skipped(), 0);

        // A healthy bucket grants the full ask untouched — the floor must be
        // invisible on the path that matters most.
        let mut healthy = RemoteBudget::new(100_000, MIN_VIABLE_MAP_GRANT);
        assert_eq!(healthy.admit_reserve(4096), Some(4096));
        assert_eq!(healthy.skipped(), 0);
    }

    #[test]
    fn map_remote_bucket_zero_budget_is_exhausted_from_the_first_item() {
        // The hard opt-out: a 0 allowance refuses every hosted call, the same
        // as `admit_remote_execution` refuses a single hosted dispatch.
        let mut b = RemoteBudget::new(0, MIN_VIABLE_MAP_GRANT);
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
        let mut b = RemoteBudget::new(100_000, MIN_VIABLE_MAP_GRANT);
        assert_eq!(b.remaining(), 100_000);
        let g = b.admit_reserve(70_000).expect("admits");
        b.settle(g, 70_000, 1);
        assert_eq!(b.remaining(), 30_000, "a later item's grant clamps to 30k, not 100k");
        assert_eq!(
            b.admit_reserve(40_960).expect("still admits"),
            30_000,
            "the grant reads the remaining allowance"
        );
        b.settle(30_000, 60_000, 1); // overshoot: the endpoint reported above its grant
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
            reasoning_tokens: None,
            cached_tokens: None,
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
            total_tokens: Some(1261),
            prompt_tokens: Some(75),
            completion_tokens: Some(1186),
            reasoning_tokens: Some(1024),
            cached_tokens: Some(64),
            model: Some("hosted".to_string()),
        };
        let payload = hosted_single_shot_step_payload("s1", 500_000, 4096, 4096, &reply);
        assert_eq!(payload["reasoning_tokens"], 1024);
        assert_eq!(payload["cached_tokens"], 64);
        // Neighbors, so a copy-paste slip between fields cannot pass.
        assert_eq!(payload["prompt_tokens"], 75);
        assert_eq!(payload["completion_tokens"], 1186);
        assert_eq!(payload["total_tokens"], 1261);
        assert_eq!(payload["step_id"], "s1");
        assert_eq!(payload["kind"], "dispatch.single_shot");
        assert_eq!(payload["runtime"], "direct");
        assert_eq!(payload["max_tokens_sent"], 4096);
    }

    /// (#1444 review) An endpoint that reported no details object leaves
    /// both keys PRESENT-and-null — never absent, never `0`. The runtime-
    /// side `telemetry.tokens` producers use the same null convention, so a
    /// consumer reading this family sees one answer for "didn't say".
    #[test]
    fn single_shot_step_telemetry_renders_unreported_details_as_null() {
        let reply = crate::single_shot::SingleShotReply {
            content: "ok".to_string(),
            total_tokens: Some(42),
            prompt_tokens: Some(30),
            completion_tokens: Some(12),
            reasoning_tokens: None,
            cached_tokens: None,
            model: None,
        };
        let payload = hosted_single_shot_step_payload("s1", 500_000, 4096, 4096, &reply);
        let obj = payload.as_object().expect("object");
        assert!(obj.contains_key("reasoning_tokens"), "the key stays present");
        assert!(payload["reasoning_tokens"].is_null(), "null, never a fabricated 0");
        assert!(obj.contains_key("cached_tokens"));
        assert!(payload["cached_tokens"].is_null());
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
        let payload = map_item_token_payload(&res).expect("a reply with usage emits a record");
        assert_eq!(payload["total_tokens"], 4547);
        assert_eq!(payload["prompt_tokens"], 2490, "GENERATED/fresh/re-read read the split");
        assert_eq!(payload["completion_tokens"], 2057);
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
        let payload = map_item_token_payload(&res).expect("a reply with usage emits a record");
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
        let payload = map_item_token_payload(&res).expect("emits");
        assert_eq!(payload["reasoning_tokens"], 300);
        assert!(
            payload.get("cached_tokens").is_none(),
            "an unreported field is omitted, never zeroed — and never dragged \
             along by its sibling being present"
        );

        let neither = MapItemResult { reasoning_tokens: None, cached_tokens: None, ..res };
        let payload = map_item_token_payload(&neither).expect("emits");
        assert!(payload.get("reasoning_tokens").is_none());
        assert!(payload.get("cached_tokens").is_none());
    }

    /// (#1444 review) The accumulator behind those fields. Independent flags
    /// are the point: a provider can name `completion_tokens_details`
    /// without `prompt_tokens_details` (the runtime's own two-turn
    /// accumulator test pins exactly that turn shape), so a shared
    /// `any_*` flag would fabricate a `0` for whichever one went unnamed.
    #[test]
    fn accumulate_details_tracks_each_field_independently() {
        let (mut r, mut c, mut any_r, mut any_c) = (0u64, 0u64, false, false);

        // No attempt reported anything → both stay absent.
        accumulate_details(None, None, &mut r, &mut c, &mut any_r, &mut any_c);
        assert_eq!(item_split_tokens(any_r, r), None);
        assert_eq!(item_split_tokens(any_c, c), None);

        // Attempt 1 reports both; attempt 2 reports reasoning only.
        accumulate_details(Some(500), Some(20), &mut r, &mut c, &mut any_r, &mut any_c);
        accumulate_details(Some(300), None, &mut r, &mut c, &mut any_r, &mut any_c);
        assert_eq!(item_split_tokens(any_r, r), Some(800), "500 + 300 across attempts");
        assert_eq!(
            item_split_tokens(any_c, c),
            Some(20),
            "attempt 2's silence on cached must not reset or zero what attempt 1 reported"
        );

        // The mirror case: cached reported, reasoning never.
        let (mut r2, mut c2, mut any_r2, mut any_c2) = (0u64, 0u64, false, false);
        accumulate_details(None, Some(64), &mut r2, &mut c2, &mut any_r2, &mut any_c2);
        assert_eq!(
            item_split_tokens(any_r2, r2),
            None,
            "a shared flag would have fabricated Some(0) here"
        );
        assert_eq!(item_split_tokens(any_c2, c2), Some(64));
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
            reasoning_tokens: None,
            cached_tokens: None,
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
            reasoning_tokens: None,
            cached_tokens: None,
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
            MapItemResult { index: 0, ok: true, content: "a".to_string(), error: None, total_tokens: Some(100), prompt_tokens: None, completion_tokens: None, reasoning_tokens: None, cached_tokens: None, served_model: None, wall_ms: 0, retried: 0 },
            MapItemResult { index: 1, ok: false, content: String::new(), error: Some("boom".to_string()), total_tokens: None, prompt_tokens: None, completion_tokens: None, reasoning_tokens: None, cached_tokens: None, served_model: None, wall_ms: 0, retried: 0 },
            MapItemResult { index: 2, ok: true, content: "c".to_string(), error: None, total_tokens: Some(250), prompt_tokens: None, completion_tokens: None, reasoning_tokens: None, cached_tokens: None, served_model: None, wall_ms: 0, retried: 0 },
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

        let clean = vec![MapItemResult { index: 0, ok: true, content: "a".to_string(), error: None, total_tokens: Some(5), prompt_tokens: None, completion_tokens: None, reasoning_tokens: None, cached_tokens: None, served_model: None, wall_ms: 0, retried: 0 }];
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
            reasoning_tokens: None,
            cached_tokens: None,
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
                reasoning_tokens: None,
                cached_tokens: None,
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
                    reasoning_tokens: None,
                    cached_tokens: None,
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
                reasoning_tokens: None,
                cached_tokens: None,
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
        // The batched `run()` path deliberately emits none (see StepBookend).
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
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL
    fn dispatch_map_local_streaming_bookends_carry_the_namespaced_identifier() {
        // (MUST FIX 5) The LOCAL twin of the hosted bookend test above —
        // `dispatch.map`'s bookends, like `dispatch.single_shot`'s, are only
        // observable through the streaming channel (`StepBookend::new` only
        // emits through a `ctx`; the batched `run()` path is inert for
        // them). Reverting either `Self::bookend_record(step, wire_model.
        // as_ref(), ...)` call in `run_map` back to the bare `model` leaves
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
        let ctx = StepRunCtx::new(
            Some(tx),
            None,
            None,
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );
        let result = DispatchMapStepKind.run_streaming(&s, &empty_task(), &BTreeMap::new(), &ctx);
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
            emitted.iter().filter(|r| r.action.starts_with("dispatch ")).collect();
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
                total_tokens: Some(5),
                prompt_tokens: None,
                completion_tokens: None,
                reasoning_tokens: None,
                cached_tokens: None,
                model: None,
            })
        });

        let s = map_step(json!({
            "model": "qwen3.6-35b",
            "user_template": "check {item}",
            "collection": ["a"],
        }));
        let ctx = StepRunCtx::new(None, None, Some(ovr), Arc::new(crate::step_kinds::ArtifactBus::new()));
        DispatchMapStepKind.run_streaming(&s, &empty_task(), &BTreeMap::new(), &ctx).unwrap();

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
            darkmux_types::session_id::task("t1")
        );
        let entries = log.lock().unwrap().clone();
        let set_count =
            entries.iter().filter(|e| e.contains("SET") && e.contains(&expected_key)).count();
        let saw_claim_edge = entries.iter().any(|e| e.contains("edge-claim:session-end:task-t1"));
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
        let out = DispatchMapStepKind.run(&s, &empty_task(), &BTreeMap::new());

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
        let ctx = StepRunCtx::new(
            Some(tx),
            None,
            None,
            std::sync::Arc::new(crate::step_kinds::ArtifactBus::new()),
        );
        let out = DispatchSingleShotStepKind
            .run_streaming(&s, &empty_task(), &BTreeMap::new(), &ctx)
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
            darkmux_types::session_id::task("t1")
        );
        let entries = log.lock().unwrap().clone();
        assert!(
            entries.iter().any(|e| e.contains("SET") && e.contains(&expected_key)),
            "a hosted `dispatch.single_shot` must beat while it is generating — expected a \
             session-presence SET for {expected_key}; saw {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e.contains("edge-claim:session-end:task-t1")),
            "expected `stop()`'s session-end edge claim, proving the beat was released rather \
             than left to TTL out; saw {entries:?}"
        );

        // Bookends — the half this kind never had at all.
        let actions: Vec<String> = rx
            .into_iter()
            .filter_map(|sig| match sig {
                crate::step_kinds::WaveSignal::Record(r) => Some(r.action),
                _ => None,
            })
            .collect();
        assert_eq!(
            actions.iter().filter(|a| *a == "dispatch start").count(),
            1,
            "exactly one liveness start; got {actions:?}"
        );
        assert_eq!(
            actions
                .iter()
                .filter(|a| a.starts_with("dispatch ") && *a != "dispatch start")
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
        let out = DispatchSingleShotStepKind.run(&s, &empty_task(), &BTreeMap::new());

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
                    reasoning_tokens: None,
                    cached_tokens: None,
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
                    PlacementMiss::Remote => "remote".to_string(),
                    PlacementMiss::ResolutionFailed(r) => r,
                })
        };
        assert_eq!(pick(None, None), Ok("m-default".into()), "unbound → default_profile");
        assert_eq!(pick(None, Some("q")), Ok("m-mapped".into()), "the role_profiles binding wins over the default");
        assert_eq!(pick(Some("p"), Some("q")), Ok("m-default".into()), "an explicit profile wins over the binding");
        let err = pick(None, Some("nope")).unwrap_err();
        assert!(err.contains("nope"), "a binding to an undefined profile is a loud error naming it, never a silent fallback: {err}");
    }
}
