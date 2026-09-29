//! (#2914) darkmux's own jobs on the machine's ONE utility model, run LEAN.
//!
//! The machine has one utility model, `internal.utility` in
//! `profiles.json`, declared once with its own window. darkmux's own jobs
//! run on it and nothing else does: every task/step selection path sets
//! it aside (`crate::select`), so a profile's `models[]` are work models
//! only. Which jobs are utility is defined ONCE, by
//! [`crate::usage::call_purpose`]: every runtime compactor call, and every
//! call made by the radio ROUTING role. This module is the host-side half
//! of that list (routing); compaction runs inside the container runtime,
//! on the same binding, and lands its usage record through the trajectory
//! tailer (`dispatch_internal::compaction_call_tokens_payload`).
//!
//! **Lean means:** a utility job emits its `telemetry.tokens` usage record
//! (`purpose: utility`, `handle` = the job's role id) and NOTHING else. No
//! session is minted, no `dispatch start`/`dispatch complete` bookends are
//! written, no presence beat runs, and the runs board lists nothing. That
//! is the amended contract 2 (CLAUDE.md, "Dispatch liveness"): bookends
//! are for WORK executions. The usage record's `session_id` is absent, so
//! no session-keyed reader (the runs board's ghost rows, the fleet card's
//! activity, the status line's last dispatch) ever sees it; the fleet
//! hero's plain sum still counts it, under its own utility chip.
//!
//! **Visible (#2915).** [`run_utility_single_shot`] is the ONE chokepoint a
//! host-side utility job passes through, and [`UtilityJob::role_id`] names
//! the job ([`crate::usage::utility_job`] maps it to its
//! [`crate::usage::UtilityJobKind`]). It writes a lean `utility.start`
//! right before the model call; the usage record marks the end, or a
//! `utility.error` when the call fails. The container half (compaction)
//! gets the same `utility.start` from the trajectory tailer, mapped from
//! the runtime's `compaction.start` event.
//!
//! **One instance, and the wait is accepted.** LM Studio serves one
//! request per instance at a time, so a routing call fired during a long
//! compaction waits behind it (measured 2026-09-26: 13.9s behind a real
//! compaction, on the issue). The operator chose the wait over a second
//! instance or parallel slots; #2915 makes the pause visible. Do not add
//! slots or a second instance here.

use anyhow::{anyhow, bail, Context, Result};
use darkmux_types::execution_id::ExecutionId;

/// One utility job: a single model call on the machine's utility model.
#[derive(Debug, Clone, Copy)]
pub struct UtilityJob<'a> {
    /// The job's ROLE id (`crate::loader::RADIO_ROUTER_ROLE_ID` for
    /// routing). Names the system prompt (loaded from the role library like
    /// any other role's), the usage record's `handle`, and, through
    /// [`crate::usage::call_purpose`], the record's `purpose`.
    pub role_id: &'a str,
    /// The user message.
    pub message: &'a str,
    /// A bounded ceiling for a bounded job (the curl `-m`).
    pub timeout_seconds: u32,
    /// The completion cap.
    pub max_tokens: u32,
    /// `--profiles-file` override for the registry that holds the binding.
    pub config_path: Option<&'a str>,
    /// A non-LMStudio base URL (the mock harness). Skips the residency
    /// preflight and puts the BARE binding id on the wire, exactly as the
    /// work primitives do for a non-LMStudio base (there is no darkmux
    /// instance to address there).
    pub base_url_override: Option<&'a str>,
}

/// What a utility job hands back: the reply text, nothing else.
#[derive(Debug, Clone)]
pub struct UtilityReply {
    pub content: String,
}

/// (#2914) The window a binding that declares no `n_ctx` is loaded
/// at for a host-side utility job. Named, and disclosed on stderr when it
/// applies, never silently substituted (#44). 16K is the window the radio
/// routing seat ran at before #2914 (the operator's `radio` profile); a
/// compaction that later needs more reloads at its own window through the
/// same residency helper (`ctx_sufficient`). Declaring `n_ctx` in
/// `internal.utility` is the fix, and `darkmux doctor` says so.
pub const UNDECLARED_UTILITY_WINDOW: u32 = 16_384;

/// The window a host-side utility job loads the binding at, and the
/// disclosure to print when it is the fallback: the binding's declared
/// `n_ctx` (no disclosure), else [`UNDECLARED_UTILITY_WINDOW`] with a line
/// naming the binding, the window, the job, and the fix. Pure, so the
/// fallback and its wording are pinned by a test (#2914 review, C7).
pub(crate) fn utility_load_window(binding_id: &str, declared_n_ctx: Option<u32>, role_id: &str) -> (u32, Option<String>) {
    match declared_n_ctx {
        Some(n) => (n, None),
        None => (
            UNDECLARED_UTILITY_WINDOW,
            Some(format!(
                "darkmux: utility model `{binding_id}` declares no `n_ctx` in `internal.utility`; \
                 loading it at {UNDECLARED_UTILITY_WINDOW} for `{role_id}` as a fallback. Declare it once: \
                 `\"internal\": {{ \"utility\": {{ \"id\": \"{binding_id}\", \"n_ctx\": N }} }}`. (#2914)"
            )),
        ),
    }
}

/// Run one utility job on the machine's utility model, lean (see the
/// module doc for what lean means and why).
///
/// Errors, with nothing recorded: no `internal.utility` binding (the fix
/// is named), an unknown role, a role with no readable prompt, or a
/// residency load failure. The model call itself failing (a vanished
/// instance is re-worded by `residency_lost_detail`, as on the work path)
/// happens after the job's `utility.start`, so it records the matching
/// `utility.error` (#2915) and no usage record: a failed call spent no
/// countable tokens. The caller (radio's router) turns any error into a
/// refusal.
pub fn run_utility_single_shot(job: &UtilityJob<'_>) -> Result<UtilityReply> {
    let Some((binding_id, declared_n_ctx)) =
        crate::dispatch_internal::resolve_utility_model_internal(job.config_path)
    else {
        bail!(
            "darkmux: no machine utility model is registered, and `{}` is a utility job that runs \
             only on it. Register one as `internal.utility` in ~/.darkmux/profiles.json: \
             `\"internal\": {{ \"utility\": {{ \"id\": \"<model-id>\", \"n_ctx\": <window> }} }}` \
             (a small instruct model; a 4B is ideal). `darkmux doctor` shows the binding. (#2914)",
            job.role_id
        );
    };

    let roles = crate::loader::load_roles().context("loading roles for a utility job")?;
    let role = roles
        .iter()
        .find(|r| r.id == job.role_id)
        .ok_or_else(|| anyhow!("role not found: {}", job.role_id))?;
    let role_prompt = crate::loader::load_role_prompt_for(role).ok_or_else(|| {
        anyhow!(
            "role '{}' has no readable .md system prompt (checked prompt_path={:?}, the conventional \
             roles dir, and the embedded table) — a utility job requires one",
            job.role_id,
            role.prompt_path
        )
    })?;
    // Same prompt assembly as the work single-shot primitive: the
    // autonomous-dispatch preamble is a SPECIALIST's (a utility-family role
    // gets none), and the operator-identity section applies to every local
    // brain (#147).
    let system_prompt = if role.is_specialist() {
        format!("{}\n\n{}", crate::loader::load_autonomous_dispatch_preamble().trim_end(), role_prompt)
    } else {
        role_prompt
    };
    let system_prompt = crate::dispatch::augment_prompt_with_identity(&system_prompt);

    // Residency + the wire id, mirroring the compactor's own treatment
    // (`apply_compactor_residency`): the binding is loaded at ITS declared
    // window under the `darkmux:` namespace, and the SAME namespaced
    // identifier goes on the wire (#2240). A non-LMStudio base skips both.
    let wire_model = match job.base_url_override {
        Some(_) => binding_id.clone(),
        None => {
            let (window, disclosure) = utility_load_window(&binding_id, declared_n_ctx, job.role_id);
            if let Some(line) = disclosure {
                eprintln!("{line}");
            }
            let source = if declared_n_ctx.is_some() {
                crate::dispatch_internal::WindowSource::UtilityBinding
            } else {
                crate::dispatch_internal::WindowSource::UtilityFallback
            };
            crate::dispatch_internal::ensure_model_loaded_at_ctx_from(
                &crate::dispatch_internal::utility_residency_pm(&binding_id, window),
                source,
            )
            .with_context(|| format!("loading the utility model `{binding_id}` for `{}`", job.role_id))?;
            crate::dispatch_internal::compactor_wire_model_id(&binding_id)
        }
    };

    let req = crate::single_shot::SingleShotRequest {
        base_url: job.base_url_override,
        model: &wire_model,
        system: &system_prompt,
        user: job.message,
        // A bounded classification wants a low-variance decision, the same
        // setting the work single-shot primitive uses.
        temperature: 0.2,
        max_tokens: job.max_tokens,
        timeout_seconds: job.timeout_seconds,
    };
    // (#2915) The job is VISIBLE while it runs: one lean `utility.start`
    // (no session) right before the call, so the viewer's fleet card shows
    // the job from here until its usage record (or `utility.error`) lands.
    // Emitted after residency on purpose: a job that failed to load never
    // started, and must not leave a start with no end behind.
    // (#2915 review) Its own job id, echoed by its end, and ms times.
    let job_kind = crate::usage::utility_job(crate::usage::CallKind::SingleShot, Some(job.role_id));
    let job_id = job_kind.map(crate::usage::mint_utility_job_id).unwrap_or_default();
    let started_at_ms = crate::usage::unix_ms_now();
    // (#2928) The job's two edges also go out on the live channel, at once:
    // a sub-second routing job's durable start and end otherwise reach the
    // viewer in the same delivery and it is never drawn open.
    let mut live = job_kind.and_then(|_| darkmux_flow::live::LiveSender::for_local_daemon());
    if let Some(kind) = job_kind {
        let payload = crate::usage::utility_start_payload(kind, &job_id, &wire_model, None, u64::from(job.timeout_seconds).saturating_mul(1_000), started_at_ms);
        if let Some(tx) = live.as_mut() {
            let fields = serde_json::to_value(&payload).unwrap_or_default();
            tx.send(&live_utility_edge(&fields, "start", job.role_id, &wire_model, started_at_ms));
        }
        let _ = darkmux_flow::record(crate::usage::utility_marker_record(
            job.role_id,
            &wire_model,
            darkmux_flow::Payload::UtilityStart(payload),
        ));
    }
    let send_live_end = |live: &mut Option<darkmux_flow::live::LiveSender>, ended_at_ms: u64| {
        if let (Some(tx), Some(kind)) = (live.as_mut(), job_kind) {
            let fields = serde_json::json!({
                "job": kind,
                "job_id": job_id,
                "ended_at_ms": ended_at_ms,
                "duration_ms": ended_at_ms.saturating_sub(started_at_ms),
            });
            tx.send(&live_utility_edge(&fields, "end", job.role_id, &wire_model, ended_at_ms));
        }
    };
    let reply = match crate::single_shot::single_shot_chat(&req) {
        Ok(r) => r,
        Err(e) => {
            send_live_end(&mut live, crate::usage::unix_ms_now());
            // (#2915) The end of a started job that has no usage record.
            if let Some(kind) = job_kind {
                let payload = crate::usage::utility_error_payload(
                    kind,
                    &wire_model,
                    &job_id,
                    started_at_ms,
                    crate::usage::unix_ms_now(),
                );
                let _ = darkmux_flow::record(crate::usage::utility_marker_record(
                    job.role_id,
                    &wire_model,
                    darkmux_flow::Payload::UtilityError(payload),
                ));
            }
            return Err(match crate::dispatch_internal::residency_lost_detail(&wire_model, &format!("{e:#}")) {
                Some(msg) => e.context(msg),
                None => e,
            });
        }
    };

    // The one record: the job's usage, attributed to the job's role, on the
    // utility model, with no session.
    let mut payload = reply.usage_payload(
        crate::usage::CallKind::SingleShot,
        Some(job.role_id),
        &wire_model,
        &crate::usage::lmstudio_endpoint(job.base_url_override),
        None,
    );
    if job_kind.is_some() {
        let ended_at_ms = crate::usage::unix_ms_now();
        send_live_end(&mut live, ended_at_ms);
        crate::usage::stamp_utility_end(&mut payload, &job_id, started_at_ms, ended_at_ms);
    }
    let _ = darkmux_flow::record(crate::usage::utility_usage_record(job.role_id, &wire_model, &ExecutionId::mint(), payload));

    Ok(UtilityReply { content: reply.content })
}

/// (#2928) One edge (`start` | `end`) of a host-side utility job as a live
/// sample: the job's own fields plus `event`, attributed to the job's role
/// and model, with no session (routing serves none).
fn live_utility_edge(
    fields: &serde_json::Value,
    event: &str,
    role_id: &str,
    model: &str,
    at_ms: u64,
) -> darkmux_flow::live::LiveSample {
    let mut s = darkmux_flow::live::LiveSample::new(
        darkmux_flow::live::LiveKind::Utility,
        at_ms,
        darkmux_types::config_access::live_sample_ms(),
    );
    s.role = Some(role_id.to_string());
    s.model = Some(model.to_string()).filter(|m| !m.is_empty());
    if let Some(map) = fields.as_object() {
        s.fields = map.clone();
    }
    s.fields.insert("event".into(), serde_json::json!(event));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (#2928) A routing job's live edges carry the job, its id and the
    /// event, and no session.
    #[test]
    fn live_utility_edge_names_the_job_and_the_event() {
        let start = crate::usage::utility_start_payload(crate::usage::UtilityJobKind::RadioRouting, "j-9", "u4b", None, 30_000, 1_000);
        let s = live_utility_edge(&serde_json::to_value(&start).unwrap(), "start", "radio-router", "u4b", 1_000);
        assert_eq!(s.kind, darkmux_flow::live::LiveKind::Utility);
        assert_eq!((s.fields["event"].as_str(), s.fields["job"].as_str(), s.fields["job_id"].as_str()), (Some("start"), Some("radio_routing"), Some("j-9")));
        assert_eq!(s.session_id, None, "routing serves no execution");
        assert_eq!((s.role.as_deref(), s.model.as_deref(), s.at_ms), (Some("radio-router"), Some("u4b"), 1_000));
        let bytes = s.to_bytes().unwrap();
        assert!(darkmux_flow::live::LiveSample::from_datagram(&bytes).is_some(), "the daemon accepts it");
    }

    /// (#2914 review, C7) The window a host-side utility job loads at: the
    /// binding's declared `n_ctx`, silently; else `UNDECLARED_UTILITY_WINDOW`
    /// with a disclosure naming the binding, the window, the job and the
    /// fix (operator sovereignty, #44: a fallback is never silent).
    #[test]
    fn utility_load_window_declares_the_fallback() {
        assert_eq!(utility_load_window("util-4b", Some(120_000), "radio-router"), (120_000, None));
        let (window, disclosure) = utility_load_window("darkmux:util-4b", None, "radio-router");
        assert_eq!(window, UNDECLARED_UTILITY_WINDOW);
        let text = disclosure.expect("an undeclared window is disclosed");
        assert!(text.contains("darkmux:util-4b") && text.contains("16384") && text.contains("radio-router"), "{text}");
        assert!(text.contains("n_ctx") && text.contains("internal.utility"), "names the fix: {text}");
    }
}
