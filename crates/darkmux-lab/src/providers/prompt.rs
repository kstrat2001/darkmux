//! Trivial workload provider: "just answer this prompt."
//! No sandbox; optional must_contain / must_not_contain keyword checks, and
//! an optional structured-output check of the reply (`verify.expect`).
//!
//! Dispatches via darkmux's in-house internal runtime.

use darkmux_types::Profile;
use crate::lab::dispatch_end::{DispatchEnd, Dispatched};
use crate::lab::manifest::{ManifestVerify, ManifestVerifyReport, RunManifest};
use crate::providers::expect;
use crate::workloads::types::{
    InspectionReport, LoadedWorkload, ReplyRoom, RunResult, VerifyOutcome, VerifyReport, WorkloadProvider,
};
use anyhow::{anyhow, Context, Result};
use darkmux_types::session_id::{RunId, SessionId};
#[cfg(test)]
use std::env;
use std::fs;
use std::path::Path;

pub(crate) struct PromptProvider;

impl WorkloadProvider for PromptProvider {
    fn id(&self) -> &'static str {
        "prompt"
    }
    fn dispatch_role(&self, loaded: &LoadedWorkload) -> Option<String> {
        Some(pick_role(loaded))
    }

    fn setup(&self, _loaded: &LoadedWorkload, run_dir: &Path, _sandbox_dir: &Path) -> Result<()> {
        if !run_dir.exists() {
            fs::create_dir_all(run_dir)
                .with_context(|| format!("creating {}", run_dir.display()))?;
        }
        Ok(())
    }

    fn run(
        &self,
        loaded: &LoadedWorkload,
        run_dir: &Path,
        sandbox_dir: &Path,
        profile: &Profile,
        profile_name: &str,
        config_path: Option<&str>,
        // (#986) The prompt provider runs trivial single-prompt workloads with
        // no compaction config to override — the loop lab targets coding-task
        // workloads. Accepted to satisfy the trait; intentionally unused.
        _loop_override: Option<&crate::lab::loop_report::LoopCompactionOverride>,
        run: &RunId,
        on_session_id: &mut dyn FnMut(&SessionId),
    ) -> Result<RunResult> {
        let prompt = resolve_prompt_from(loaded, prompt_base(loaded, sandbox_dir))?;
        let role = pick_role(loaded);
        let session_id = SessionId::adhoc(run.clone(), &role, &loaded.manifest.workload.id);
        // (#2511) Report the id back to the lab harness BEFORE dispatching —
        // this is the ONE mint for this run, so it is the run's own
        // governing dispatch session.
        on_session_id(&session_id);

        let started = std::time::Instant::now();
        let mut opts = dispatch_opts(
            &role,
            &prompt,
            &session_id,
            loaded.manifest.workload.image.as_deref(),
            config_path,
            profile_name,
            crate::providers::lab_inputs(loaded, sandbox_dir),
        );
        // Named and armed before the dispatch, so a dispatch that returns
        // `Err` still leaves its trajectory in the run dir (the guard's Drop).
        let guard = PromptDispatchGuard::arm(&role, &loaded.manifest.workload.id, run_dir, &mut opts);
        // (`replyRoom`) Worked out from the route the dispatch itself will
        // take: its selected model's window and its own system prompt.
        let route = one_call_route(&opts, &loaded.manifest.workload.id, loaded.manifest.workload.reply_room, &prompt)?;
        opts.max_completion_tokens = route.reply_cap;
        let (dispatched, out_dir) = dispatch_via_internal(opts)?;
        // Disarm before the one-call trajectory is written, so the guard
        // does not then mark that finished trajectory interrupted.
        let refused = guard.keep(out_dir.as_deref(), run_dir);
        // The container path mounts a bookkeeping dir; the one-call path has
        // none, and its trajectory is the one call's own counts.
        let one_call = out_dir.is_none();
        if one_call {
            write_one_call_trajectory(&dispatched.stdout, route.system_chars, prompt.chars().count(), run_dir, now_ms());
        }
        finish_run(
            &FinishInputs {
                loaded,
                run_dir,
                profile,
                profile_name,
                session_id: &session_id,
                duration_ms: started.elapsed().as_millis(),
                // Only the one-call path sends the cap; the container path's
                // runtime continues a reply in chunks and caps nothing.
                reply_cap: route.reply_cap.filter(|_| one_call),
                n_ctx: route.n_ctx.filter(|_| one_call),
                prompt_tokens_counted: route.prompt_tokens_counted.filter(|_| one_call),
                refused: &refused,
            },
            &dispatched,
        )
    }

    fn inspect(&self, loaded: &LoadedWorkload, run_dir: &Path) -> Result<InspectionReport> {
        let meta = RunManifest::read_or_default(run_dir)?;
        let reply_path = run_dir.join("qa-reply.json");
        let reply = if reply_path.exists() {
            extract_reply_text(&fs::read_to_string(&reply_path)?)
        } else {
            String::new()
        };
        let verify_outcome = run_verify_with(loaded, &reply, hit_length_limit(run_dir));
        let run_id = meta
            .run_id
            .clone()
            .or_else(|| {
                run_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "(unknown)".to_string());
        Ok(InspectionReport {
            run_id,
            workload_id: loaded.manifest.workload.id.clone(),
            walltime_ms: meta.duration_ms.unwrap_or(0) as u128,
            turns: 1,
            compactions: 0,
            // (#2094) A single-turn provider never rests (nothing to rest
            // BETWEEN).
            rest_ms: 0,
            mode: None,
            // (#2494) The typed twin of the note below; `None` when the
            // workload declares no verify, so a run nothing checked never
            // renders as a pass.
            verify: verify_outcome.as_ref().map(|v| crate::workloads::types::VerifyReport {
                passed: v.passed,
                details: v.details.clone(),
            }),
            notes: [format!("provider={}", self.id()), verify_note(verify_outcome.as_ref())]
                .into_iter()
                .chain(DispatchEnd::inspect_note(&meta))
                .collect(),
        })
    }
}

/// What `finish_run` needs besides the dispatch itself.
pub(crate) struct FinishInputs<'a> {
    pub loaded: &'a LoadedWorkload,
    pub run_dir: &'a Path,
    pub profile: &'a Profile,
    pub profile_name: &'a str,
    pub session_id: &'a SessionId,
    pub duration_ms: u128,
    /// The `max_completion_tokens` this run's dispatch actually sent, when
    /// the workload asked for `replyRoom` and the dispatch was one call.
    pub reply_cap: Option<u32>,
    /// The window of the model the one-call dispatch selected.
    pub n_ctx: Option<u32>,
    /// Prompt tokens counted for that window (see [`counted_prompt_tokens`]).
    pub prompt_tokens_counted: Option<u32>,
    /// Runtime files the run refused to copy. Empty leaves the manifest unchanged.
    pub refused: &'a [crate::providers::coding_task::RefusedArtifact],
}

/// Everything after the dispatch: persist its output, verify the reply, write
/// the manifest and build the result. How the dispatch ended is derived HERE
/// from the raw dispatch (never passed in), so an escalation reaches the
/// manifest and the result, and a test can drive this without a runtime.
pub(crate) fn finish_run(f: &FinishInputs<'_>, d: &Dispatched) -> Result<RunResult> {
    let end = d.end();
    fs::write(f.run_dir.join("qa-reply.json"), &d.stdout)?;
    if !d.stderr.is_empty() {
        fs::write(f.run_dir.join("qa-reply.err"), &d.stderr)?;
    }
    let reply = extract_reply_text(&d.stdout);
    let hit_length = hit_length_limit(f.run_dir);
    let verify = run_verify_with(f.loaded, &reply, hit_length);
    let recorded_verify =
        verify.as_ref().map(|v| VerifyReport { passed: v.passed, details: v.details.clone() });
    let fields = expect_report_with(f.loaded, &reply, hit_length);
    write_manifest(
        f.run_dir,
        &ManifestInputs {
            workload_id: &f.loaded.manifest.workload.id,
            profile_name: f.profile_name,
            profile_description: f.profile.description.as_deref().unwrap_or_default(),
            duration_ms: f.duration_ms,
            session_id: f.session_id,
            verify: recorded_verify.as_ref(),
            end: &end,
            refused: f.refused,
        },
    )?;
    record_structured_output(f.run_dir, fields.as_ref(), f.reply_cap)?;
    record_window(f.run_dir, f.n_ctx, f.prompt_tokens_counted)?;
    Ok(RunResult {
        escalation: end.escalation().map(str::to_string),
        ok: end.ok(),
        duration_ms: f.duration_ms,
        verify,
        error: d.error(),
    })
}

/// What a prompt run's `manifest.json` records, gathered after the dispatch so
/// the manifest can be built and written without one (F2).
pub struct ManifestInputs<'a> {
    pub workload_id: &'a str,
    pub profile_name: &'a str,
    pub profile_description: &'a str,
    pub duration_ms: u128,
    pub session_id: &'a SessionId,
    pub verify: Option<&'a VerifyReport>,
    /// How the dispatch ended; stamped onto the manifest so an escalation
    /// reads as one on every surface that reads `manifest.json`.
    pub end: &'a DispatchEnd,
    /// Runtime files the run refused to copy. Empty leaves the manifest unchanged.
    pub refused: &'a [crate::providers::coding_task::RefusedArtifact],
}

/// Build and write this run's `manifest.json`: the one place the prompt
/// provider records a run, including the dispatch's escalation.
pub fn write_manifest(run_dir: &Path, m: &ManifestInputs<'_>) -> Result<()> {
    let run_id = run_dir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    let mut manifest = RunManifest {
        // v2 added: run_id, profile (now the profile NAME), profile_description.
        // v1 had: session_id, profile (was the description text), workload, provider, duration_ms, ok.
        // v5 added: verify, the same field and version as the coding-task
        // manifest (#2494). `null` is "not checked": the workload declares
        // no verify. `darkmux run list` and `/runs` read `verify.passed`
        // from here to show a failed verify beside a good dispatch.
        // v6 (F2) may carry `escalation`, as in the coding-task manifest.
        schema_version: Some(6),
        manifest_schema_version: Some(darkmux_types::data_version::RUN_MANIFEST_SCHEMA_VERSION.to_string()),
        run_id: Some(run_id),
        workload: Some(m.workload_id.to_string()),
        provider: Some(PromptProvider.id().to_string()),
        profile: Some(m.profile_name.to_string()),
        profile_description: Some(m.profile_description.to_string()),
        duration_ms: Some(u64::try_from(m.duration_ms).unwrap_or(u64::MAX)),
        ok: Some(m.end.ok()),
        session_id: Some(m.session_id.to_string()),
        verify: Some(m.verify.map(|v| ManifestVerify::Report(Box::new(ManifestVerifyReport {
            passed: v.passed,
            details: v.details.clone(),
            work_gate: None,
            grade: None,
            extras: Default::default(),
        })))),
        ..RunManifest::default()
    };
    m.end.record_in(&mut manifest);
    manifest.refused_artifacts = crate::providers::coding_task::refused_records(m.refused);
    manifest.write(run_dir)?;
    // The copy already warned that the file was not copied. Say it was recorded
    // only now, after the field is on disk.
    crate::providers::coding_task::announce_recorded(m.refused);
    Ok(())
}

/// Stamp the structured-output count and the reply cap onto the manifest
/// `write_manifest` just wrote. The count sits in the verify object under the
/// same names for every structured-output workload (`fields_correct`,
/// `fields_expected`, `fields_missed`); the cap is top-level
/// `max_completion_tokens`. A run with neither is left as written.
fn record_structured_output(run_dir: &Path, fields: Option<&expect::FieldsReport>, reply_cap: Option<u32>) -> Result<()> {
    if fields.is_none() && reply_cap.is_none() {
        return Ok(());
    }
    let mut manifest = RunManifest::read(run_dir)?;
    if let (Some(f), Some(Some(ManifestVerify::Report(v)))) = (fields, manifest.verify.as_mut()) {
        v.extras.insert("fields_correct".into(), f.correct.into());
        v.extras.insert("fields_expected".into(), f.expected.into());
        v.extras.insert("fields_missed".into(), f.missed.clone().into());
    }
    if let Some(cap) = reply_cap {
        manifest.extras.insert("max_completion_tokens".into(), cap.into());
    }
    manifest.write(run_dir)?;
    Ok(())
}

/// Stamp the window the one-call dispatch selected, and the prompt tokens
/// counted against it, onto the manifest `write_manifest` just wrote. A run
/// that is not one call leaves both absent.
fn record_window(run_dir: &Path, n_ctx: Option<u32>, prompt_tokens_counted: Option<u32>) -> Result<()> {
    if n_ctx.is_none() && prompt_tokens_counted.is_none() {
        return Ok(());
    }
    let mut manifest = RunManifest::read(run_dir)?;
    manifest.n_ctx = n_ctx.filter(|n| *n > 0);
    manifest.prompt_tokens_counted = prompt_tokens_counted;
    manifest.write(run_dir)
}

/// The window and the counted prompt of the route this dispatch will take.
struct OneCallFacts {
    reply_cap: Option<u32>,
    system_chars: usize,
    n_ctx: Option<u32>,
    prompt_tokens_counted: Option<u32>,
}

/// Resolve the one-call route once. A workload that asks for `replyRoom`
/// surfaces a resolution error; any other workload keeps going with no
/// window when the route cannot be read, as the system-char count did.
fn one_call_route(
    opts: &darkmux_crew::dispatch::DispatchOpts,
    workload_id: &str,
    reply_room: Option<ReplyRoom>,
    prompt: &str,
) -> Result<OneCallFacts> {
    let asked = reply_room == Some(ReplyRoom::Context);
    let ctx = if asked {
        darkmux_crew::dispatch::one_call_context(opts)?
    } else {
        darkmux_crew::dispatch::one_call_context(opts).ok().flatten()
    };
    let reply_cap = if asked { reply_room_for(ctx.clone(), prompt, workload_id)? } else { None };
    let prompt_tokens_counted = ctx.as_ref().map(|c| counted_prompt_tokens(c.system_prompt_bytes + prompt.len()));
    Ok(OneCallFacts {
        reply_cap,
        system_chars: ctx.as_ref().map_or(0, |c| c.system_prompt_chars),
        n_ctx: ctx.and_then(|c| c.n_ctx),
        prompt_tokens_counted,
    })
}

/// Where a `promptFile` resolves: the run's own copy of the fixture when the
/// workload binds one (so the input is the one the recorded fixture commit
/// holds), else beside the workload document.
fn prompt_base<'a>(loaded: &'a LoadedWorkload, sandbox_dir: &'a Path) -> &'a Path {
    match loaded.manifest.workload.requires_fixture {
        Some(_) => sandbox_dir,
        None => &loaded.base_dir,
    }
}

/// Tokens of chat-template text a dispatch adds around the system prompt
/// and the message, which the byte count below cannot see.
const REPLY_ROOM_OVERHEAD_TOKENS: u32 = 512;
/// Below this the window is too small to be worth a dispatch.
const REPLY_ROOM_MIN_TOKENS: u32 = 256;

/// The reply room a `replyRoom: "context"` workload gets: the window of the
/// model the dispatch selected minus everything it sends around the reply.
/// `None` when the dispatch is not one call (`ctx` is `None`): the container
/// path continues a reply in chunks and sends no cap to lift.
fn reply_room_for(ctx: Option<darkmux_crew::dispatch::OneCallContext>, prompt: &str, workload_id: &str) -> Result<Option<u32>> {
    let Some(ctx) = ctx else { return Ok(None) };
    let n_ctx = ctx.n_ctx.ok_or_else(|| {
        anyhow!(
            "workload \"{workload_id}\" asks for replyRoom \"context\", but its model `{}` declares no n_ctx in the profile",
            ctx.model
        )
    })?;
    let room = reply_room_from(n_ctx, ctx.system_prompt_bytes + prompt.len(), workload_id)?;
    match ctx.max_output_tokens {
        Some(max) if max < REPLY_ROOM_MIN_TOKENS => Err(anyhow!(
            "workload \"{workload_id}\": model `{}` declares max_output_tokens {max}, below the {REPLY_ROOM_MIN_TOKENS} a reply needs",
            ctx.model
        )),
        Some(max) => Ok(Some(room.min(max))),
        None => Ok(Some(room)),
    }
}

/// One token per two UTF-8 bytes, plus the chat-template overhead.
fn counted_prompt_tokens(prompt_bytes: usize) -> u32 {
    u32::try_from(prompt_bytes.div_ceil(2)).unwrap_or(u32::MAX).saturating_add(REPLY_ROOM_OVERHEAD_TOKENS)
}

/// The host has no tokenizer, so the prompt is counted at one token per two
/// UTF-8 bytes, plus a fixed overhead for the chat template. English prose
/// runs nearer four bytes a token and HTML, JSON or code nearer two or three,
/// so this over-counts what lab prompts are made of and the reply gets a
/// little less than the true remainder. It is an estimate, not a bound: a
/// prompt that tokenizes worse than two bytes a token (dense CJK, emoji runs)
/// can leave less room than the cap asks for, and the server then refuses or
/// truncates. One token per byte would be a bound, but it refuses a ~23 KB
/// extraction prompt on a 16K window that actually holds it with room to
/// spare (6,144 tokens on a 1.7B model).
fn reply_room_from(n_ctx: u32, prompt_bytes: usize, workload_id: &str) -> Result<u32> {
    let prompt_tokens = counted_prompt_tokens(prompt_bytes);
    let room = n_ctx.saturating_sub(prompt_tokens);
    if room < REPLY_ROOM_MIN_TOKENS {
        return Err(anyhow!(
            "workload \"{workload_id}\": the prompt (~{prompt_tokens} tokens counted) leaves {room} of the model's {n_ctx}-token window for the reply; raise the profile's n_ctx"
        ));
    }
    Ok(room)
}

/// Dispatch via darkmux's internal Docker-bounded runtime through the
/// crew::dispatch substrate. The runtime emits a JSON envelope per
/// `runtime/src/main.rs::build_json_envelope` which becomes the
/// provider's stdout artifact.
fn dispatch_via_internal(
    opts: darkmux_crew::dispatch::DispatchOpts,
) -> Result<(Dispatched, Option<std::path::PathBuf>)> {
    let result = darkmux_crew::dispatch::dispatch(opts).context("internal-runtime dispatch via lab harness")?;
    Ok((Dispatched { exit_code: result.exit_code, stdout: result.stdout, stderr: result.stderr }, result.out_dir))
}

/// Copy the runtime's trajectory and findings into the run dir, as the
/// coding-task provider does (#364): the run's numbers (`darkmux run stats`,
/// `lab submit`) are read from the trajectory, and the dispatch's own out
/// dir is a temp dir. Returns anything the copy refused, so the caller can
/// record it. The one-call path has no out dir and writes no trajectory.
fn preserve_trajectory(out_dir: Option<&Path>, run_dir: &Path) -> Vec<crate::providers::coding_task::RefusedArtifact> {
    let Some(out) = out_dir else { return Vec::new() };
    crate::providers::coding_task::preserve_runtime_artifacts(out, run_dir, &["trajectory.jsonl", "findings.jsonl"]).refused
}

/// Names the out dir and arms preservation before a prompt dispatch. Drop
/// copies the runtime's trajectory and findings into the run dir, closes an
/// unterminated trajectory, and writes any refused copy onto the run manifest.
/// That is the only copy a dispatch that returns `Err` gets: the lab writes
/// no manifest for that outcome. [`Self::keep`] is the success path: it
/// copies, returns what was refused, and disarms before a one-call trajectory
/// is written, so Drop does not record the refusal a second time.
struct PromptDispatchGuard {
    preserve: crate::providers::coding_task::TrajectoryPreserver,
    run_dir: std::path::PathBuf,
    workload_id: String,
}

impl PromptDispatchGuard {
    fn arm(role: &str, workload_id: &str, run_dir: &Path, opts: &mut darkmux_crew::dispatch::DispatchOpts) -> Self {
        let out_dir = crate::providers::coding_task::named_out_dir(role);
        let preserve = crate::providers::coding_task::TrajectoryPreserver::new(out_dir.clone(), run_dir.to_path_buf());
        opts.host_out = Some(out_dir);
        Self { preserve, run_dir: run_dir.to_path_buf(), workload_id: workload_id.to_string() }
    }

    fn keep(mut self, dispatch_out: Option<&Path>, run_dir: &Path) -> Vec<crate::providers::coding_task::RefusedArtifact> {
        let refused = preserve_trajectory(dispatch_out, run_dir);
        self.preserve.disarm();
        refused
    }
}

impl Drop for PromptDispatchGuard {
    fn drop(&mut self) {
        let preserved = self.preserve.salvage();
        if let Err(e) = write_refusal_manifest(&self.run_dir, &self.workload_id, &preserved.refused) {
            eprintln!("darkmux: warn: could not record a refused runtime file on the run manifest: {e}");
        }
    }
}

/// The error path's manifest: `finish_run` never runs, so this is the only
/// writer. A dispatch that copied cleanly leaves no manifest. A refusal is
/// described as recorded only after this write succeeds.
fn write_refusal_manifest(run_dir: &Path, workload_id: &str, refused: &[crate::providers::coding_task::RefusedArtifact]) -> Result<()> {
    if refused.is_empty() {
        return Ok(());
    }
    let mut manifest = RunManifest::read_or_default(run_dir)?;
    manifest.refused_artifacts = crate::providers::coding_task::refused_records(refused);
    fill_refusal_defaults(&mut manifest, run_dir, workload_id);
    manifest.write(run_dir)?;
    crate::providers::coding_task::announce_recorded(refused);
    Ok(())
}

/// What a refusal manifest says that the manifest does not say already.
fn fill_refusal_defaults(manifest: &mut RunManifest, run_dir: &Path, workload_id: &str) {
    if manifest.schema_version.is_none() {
        manifest.schema_version = Some(6);
    }
    if manifest.manifest_schema_version.is_none() {
        manifest.manifest_schema_version = Some(darkmux_types::data_version::RUN_MANIFEST_SCHEMA_VERSION.to_string());
    }
    // `lab inspect` resolves a manifest's workload to read it.
    if manifest.workload.is_none() {
        manifest.workload = Some(workload_id.to_string());
    }
    if manifest.provider.is_none() {
        manifest.provider = Some(PromptProvider.id().to_string());
    }
    if manifest.run_id.is_none() {
        manifest.run_id = run_dir.file_name().and_then(|n| n.to_str()).map(str::to_string);
    }
    if manifest.ok.is_none() {
        manifest.ok = Some(false);
    }
}

/// The trajectory of a one-call run, which has no runtime to write one: a
/// header, the call's start, its one `model.completed` with the usage the
/// endpoint reported, and its completion, all from the envelope the call
/// returned (`DirectDispatchEnvelope`). The finish reason is the envelope's,
/// and empty when it names none, never guessed. A reply that is not that
/// envelope writes nothing.
fn write_one_call_trajectory(stdout: &str, system_chars: usize, prompt_chars: usize, run_dir: &Path, end_ms: u64) {
    use darkmux_trajectory::event::{
        DispatchComplete, DispatchStart, Header, ModelCompleted, StreamingEnd, StreamingStart, TrajectoryEvent,
    };
    let Ok(envelope) = serde_json::from_str::<serde_json::Value>(stdout) else { return };
    let Some(metrics) = envelope.get("metrics") else { return };
    let (Some(model), Some(wall_ms)) = (metrics.get("model").and_then(|v| v.as_str()), metrics.get("wall_ms").and_then(|v| v.as_u64())) else {
        return;
    };
    let usage: darkmux_trajectory::usage::Usage = serde_json::from_value(metrics.clone()).unwrap_or_default();
    let start_ms = end_ms.saturating_sub(wall_ms);
    let reply_chars = envelope.get("final_assistant").and_then(|v| v.as_str()).map_or(0, |s| s.chars().count() as u64);
    let marks = output_marks(metrics, start_ms, end_ms);
    let partials = marks.len() as u64;
    let mut events = vec![
        TrajectoryEvent::Header(Header { schema_version: darkmux_trajectory::TRAJECTORY_SCHEMA_VERSION.to_string() }),
        TrajectoryEvent::DispatchStart(DispatchStart {
            ts: start_ms,
            model: model.to_string(),
            system_chars: system_chars as u64,
            prompt_chars: prompt_chars as u64,
            tools: Vec::new(),
        }),
        // The call is one stream, request to reply, as a runtime stream is
        // timed: without it the run has no generation time, so no rate, and
        // its one usage frame pairs with nothing.
        TrajectoryEvent::StreamingStart(StreamingStart {
            seq: 1,
            ts: start_ms,
            system_chars: system_chars as u64,
            prompt_chars: prompt_chars as u64,
        }),
    ];
    events.extend(marks);
    events.push(TrajectoryEvent::StreamingEnd(StreamingEnd {
        seq: 1,
        ts: end_ms,
        total_content_chars: reply_chars,
        partial_count: partials,
        ..Default::default()
    }));
    events.push(TrajectoryEvent::ModelCompleted(ModelCompleted {
        seq: 1,
        ts: end_ms,
        finish_reason: metrics.get("finish_reason").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        usage: Some(usage),
        ..Default::default()
    }));
    events.push(TrajectoryEvent::DispatchComplete(DispatchComplete {
        ts: end_ms,
        result: "stop".to_string(),
        wall_ms,
        turn_delay_effective_ms: None,
    }));
    let lines: Vec<String> = events.iter().filter_map(|e| serde_json::to_string(e).ok()).collect();
    let _ = fs::write(run_dir.join(darkmux_trajectory::TRAJECTORY_FILE), lines.join("\n") + "\n");
}

/// The first-output marks a hosted stream can support. One mark when the
/// envelope says when the first text arrived; a second, at the end, when
/// more than one chunk carried text, so decode speed has an interval. No
/// mark when the call did not say, or said the first text arrived as it ended.
fn output_marks(metrics: &serde_json::Value, start_ms: u64, end_ms: u64) -> Vec<darkmux_trajectory::event::TrajectoryEvent> {
    let Some(wait) = metrics.get("first_token_ms").and_then(|v| v.as_u64()) else {
        return Vec::new();
    };
    let ts = start_ms.saturating_add(wait);
    if ts == 0 || ts >= end_ms {
        return Vec::new();
    }
    let mut marks = vec![output_mark(1, ts, 1)];
    if metrics.get("output_chunks").and_then(|v| v.as_u64()).unwrap_or(0) >= 2 {
        marks.push(output_mark(2, end_ms, 2));
    }
    marks
}

fn output_mark(index: u64, ts: u64, generated: u64) -> darkmux_trajectory::event::TrajectoryEvent {
    use darkmux_trajectory::event::{Partial, TrajectoryEvent};
    TrajectoryEvent::Partial(Partial {
        seq: 1,
        ts,
        partial_index: index,
        delta_chars: 1,
        cumulative_chars: generated,
        generated_chars: Some(generated),
        ..Partial::default()
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The lab's dispatch options for one prompt run.
fn dispatch_opts(
    role_id: &str,
    prompt: &str,
    session_id: &SessionId,
    image: Option<&str>,
    config_path: Option<&str>,
    profile_name: &str,
    lab_inputs: darkmux_crew::dispatch::LabInputs,
) -> darkmux_crew::dispatch::DispatchOpts {
    darkmux_crew::dispatch::DispatchOpts {
        finding_sites: None,
        // (#2914) The lab benchmarks candidate utility models.
        allow_utility_model: true,
        remote_origin: None,
        // (#2928) A lab run is a measurement: no live samples, no
        // sampling cost charged to the measured dispatch.
        live_channel: false,
        brief_refs: Vec::new(),
        workspace_read_only: false,
        record_context: None,
        resume_from: None,
        host_out: None,
        max_turns_override: None,
        timeout_override_seconds: None, // (#2480)
        role_id: role_id.to_string(),
        message: prompt.to_string(),
        session: session_id.clone(),
        timeout_seconds: 3600,
        skip_preflight: false,
        json: true,
        workdir: None,
        phase_id: None,
        machine: None,
        wait: true,
        // Prompt-only workloads don't accumulate context across turns
        // (single-shot dispatches); leaving compaction at runtime
        // defaults is fine. If future prompt workloads grow multi-
        // turn, derive from profile like coding_task does.
        compaction: darkmux_crew::dispatch::CompactionDispatchArgs::default(),
        // (#549/#1199) Thread the RESOLVED profile name — the profile is part
        // of the run's reproducibility key (and the scores.json artifact key),
        // so the dispatch must name it explicitly rather than re-resolving the
        // default. Matches coding_task's behavior.
        profile_name: Some(profile_name.to_string()),
        // (#984) Propagate the lab `--profiles-file` so the dispatch's
        // default-profile model resolution loads from it.
        config_path: config_path.map(str::to_string),
        // (#703 Slice 4) the workload's declared image, if any.
        // (#1199) Bench-only knobs; defaults preserve existing behavior.
        force_container: false,
        // (`replyRoom`) Set by the caller; read only by the one-call path a
        // tool-less role on an unmanaged endpoint takes. `None` keeps that
        // path's own default.
        max_completion_tokens: None,
        image: image.map(str::to_string),
        model_base_url_override: None,
        step_id: None, // (#1483) set on the graph-step path only
        system_prompt_override: None,
        // (#3363) The role from the eval, or darkmux's compiled-in one.
        lab_inputs: Some(lab_inputs),
    }
}

#[cfg(test)]
fn resolve_prompt(loaded: &LoadedWorkload) -> Result<String> {
    resolve_prompt_from(loaded, &loaded.base_dir)
}

fn resolve_prompt_from(loaded: &LoadedWorkload, base: &Path) -> Result<String> {
    if let Some(p) = loaded.manifest.workload.prompt.as_ref() {
        return Ok(p.clone());
    }
    if let Some(rel) = loaded.manifest.workload.prompt_file.as_ref() {
        let path = base.join(rel);
        return fs::read_to_string(&path)
            .with_context(|| format!("reading promptFile at {}", path.display()));
    }
    Err(anyhow!(
        "workload \"{}\" must define prompt or promptFile",
        loaded.manifest.workload.id
    ))
}

/// Resolve which darkmux role to dispatch the workload through.
///
/// Beat 36 directional principle: workloads reference DM role manifest
/// ids, not OC agent personas. Default falls back to `code-reviewer`
/// (the role best-suited to single-turn QA-flavored prompt workloads).
/// Override via the workload's `role:` field or the
/// `DARKMUX_DEFAULT_ROLE` env var.
fn pick_role(loaded: &LoadedWorkload) -> String {
    if let Some(r) = loaded.manifest.workload.role.as_deref() {
        return r.to_string();
    }
    // env(DARKMUX_DEFAULT_ROLE) > config.runtime.default_role > "code-reviewer"
    // (#661 Slice 4).
    darkmux_types::config_access::default_role().unwrap_or_else(|| "code-reviewer".to_string())
}

pub(crate) fn extract_reply_text(stdout: &str) -> String {
    if stdout.trim().is_empty() {
        return String::new();
    }
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return stdout.to_string();
    };
    // darkmux internal-runtime --json envelope (Beat 36 / Phase-A):
    // `{"final_assistant": "...", "result": "stop", ...}`.
    if let Some(final_assistant) = parsed.get("final_assistant").and_then(|v| v.as_str()) {
        return final_assistant.to_string();
    }
    if let Some(reply) = parsed.get("reply").and_then(|v| v.as_str()) {
        return reply.to_string();
    }
    String::new()
}

/// The workload's keyword verify over `text`. `None` when the workload
/// declares no verify spec: nothing was checked, which is neither a pass nor
/// a fail (#2982). This is the one place that decides it.
pub(crate) fn run_verify(loaded: &LoadedWorkload, text: &str) -> Option<VerifyOutcome> {
    run_verify_with(loaded, text, false)
}

/// [`run_verify`], told whether the reply hit its length limit
/// ([`hit_length_limit`]), so an unclosed answer is called cut off only then.
pub(crate) fn run_verify_with(loaded: &LoadedWorkload, text: &str, hit_length: bool) -> Option<VerifyOutcome> {
    let v = loaded.manifest.workload.verify.as_ref()?;
    let Some(fields) = expect_report_with(loaded, text, hit_length) else { return Some(keyword_verify(v, text)) };
    // (`verify.expect`) The structured-output check, and the keyword checks
    // too when the workload also lists keywords: both must pass.
    let keywords = (!v.must_contain.is_empty() || !v.must_not_contain.is_empty()).then(|| keyword_verify(v, text));
    let passed = fields.passed() && keywords.as_ref().is_none_or(|k| k.passed);
    let details = [Some(fields.details()), keywords.map(|k| k.details)].into_iter().flatten().collect::<Vec<_>>().join("; ");
    Some(VerifyOutcome { passed, details })
}

/// The workload's `verify.expect` check of `text`, `None` when it has none.
#[cfg(test)]
pub(crate) fn expect_report(loaded: &LoadedWorkload, text: &str) -> Option<expect::FieldsReport> {
    expect_report_with(loaded, text, false)
}

/// The workload's `verify.expect` check, told whether the reply hit its length limit.
pub(crate) fn expect_report_with(loaded: &LoadedWorkload, text: &str, hit_length: bool) -> Option<expect::FieldsReport> {
    // Only a prompt run applies it (`lab run` refuses it on any other
    // provider), so another provider's inspect never shows its verdict.
    if loaded.manifest.workload.provider != PromptProvider.id() {
        return None;
    }
    let want = loaded.manifest.workload.verify.as_ref()?.expect.as_ref()?;
    Some(expect::grade_with(want, text, hit_length))
}

/// Whether the run's last model call finished on its length limit, read from
/// the run's own trajectory (the runtime's, or the one call's). A container
/// run continues a reply cut at `length`, so its last call ends on `length`
/// only when the window itself ran out. No trajectory, or no finish reason,
/// is not a hit.
fn hit_length_limit(run_dir: &Path) -> bool {
    let Ok(raw) = fs::read_to_string(run_dir.join(darkmux_trajectory::TRAJECTORY_FILE)) else { return false };
    raw.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .rfind(|e| e.get("type").and_then(|t| t.as_str()) == Some("model.completed"))
        .and_then(|e| e.get("finish_reason").and_then(|f| f.as_str()).map(|f| f == "length" || f == "max_tokens"))
        .unwrap_or(false)
}

/// The keyword checks (`must_contain` / `must_not_contain`).
fn keyword_verify(v: &crate::workloads::types::VerifySpec, text: &str) -> VerifyOutcome {
    // (#2493, corrected by frontier review) Case sensitivity is a property
    // of WHAT is being matched, never a blanket policy — the original fix
    // lowercased unconditionally, which silently re-scoped every workload
    // that ALSO carries a `command` (demo-quickstart, quick-coding,
    // medium-coding): those manifests ask the model to quote a
    // DETERMINISTIC test runner's own output verbatim in its reply
    // ("Reply with... the OK / FAILED line"), and that keyword's casing is
    // fixed by the runner, never the model, so lowering it gains nothing
    // and actively breaks it two ways — a two-letter keyword like "ok"
    // becomes a case-insensitive substring probe against ordinary prose
    // ("looked", "book"), a false PASS; a negative keyword like "failed"
    // now matches a CORRECT reply that narrates past failure ("the suite
    // failed before my fix, it passes now"), a false FAILURE. A workload
    // with no `command` (quick-q) has no deterministic anchor at all — the
    // keyword there is checking whether the model's own free-form prose
    // engaged with a concept, and the model is free to capitalize that
    // word at a sentence boundary the prompt itself never did, which is
    // what case-insensitivity is actually for.
    //
    // So: case-sensitive (verbatim) whenever `command` is set, since only
    // THEN is the keyword standing in for a runner's own fixed-case token;
    // case-insensitive otherwise, for the free-text-only shape.
    let case_sensitive = v.command.is_some();
    let matches = |haystack: &str, needle: &str| -> bool {
        if case_sensitive {
            haystack.contains(needle)
        } else {
            haystack.to_lowercase().contains(&needle.to_lowercase())
        }
    };
    let missing: Vec<&String> = v
        .must_contain
        .iter()
        .filter(|s| !matches(text, s))
        .collect();
    let present: Vec<&String> = v
        .must_not_contain
        .iter()
        .filter(|s| matches(text, s))
        .collect();
    if missing.is_empty() && present.is_empty() {
        return VerifyOutcome {
            passed: true,
            details: "all keyword checks passed".into(),
        };
    }
    let mut bits = Vec::new();
    if !missing.is_empty() {
        bits.push(format!(
            "missing keywords: {}",
            missing
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !present.is_empty() {
        bits.push(format!(
            "disallowed keywords found: {}",
            present
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    VerifyOutcome {
        passed: false,
        details: bits.join("; "),
    }
}

/// The inspect note for a keyword verify outcome, shared by every provider
/// that reports one.
pub(crate) fn verify_note(verify: Option<&VerifyOutcome>) -> String {
    match verify {
        Some(v) if v.passed => format!("verify: ok: {}", v.details),
        Some(v) => format!("verify: fail: {}", v.details),
        None => "verify: not checked: no verify spec".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workloads::types::{VerifySpec, WorkloadManifest, WorkloadSource, WorkloadSpec};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_loaded(spec: WorkloadSpec, base_dir: PathBuf) -> LoadedWorkload {
        LoadedWorkload {
            manifest: WorkloadManifest { schema_version: None, workload: spec },
            base_dir,
            source: WorkloadSource::OnDisk,
        }
    }

    fn spec_with_prompt(prompt: &str) -> WorkloadSpec {
        WorkloadSpec {
            id: "t".into(),
            provider: "prompt".into(),
            description: None,
            role: None,
            prompt: Some(prompt.into()),
            prompt_file: None,
            sandbox_seed: None,
            setup_content: BTreeMap::new(),
            requires_external_sandbox: false,
            requires_fixture: None,
            verify: None,
            expected: None,
            image: None,
            trials: None,
            task_timeout_seconds: None,
            chain_depths: None,
            reply_room: None,
            seed: None,
            extras: BTreeMap::new(),
        }
    }

    /// (F2) `run inspect` names an escalation the provider's own manifest
    /// recorded, and says nothing for a run that did not escalate: the
    /// provider's `inspect` must read the key its own writer stamps.
    /// (F2) The value a provider records comes from the dispatch itself: an
    /// escalated dispatch (non-zero exit, escalation envelope) reaches both the
    /// result and `manifest.json`, and is not an error; a plain failure is one.
    #[test]
    fn finish_run_records_how_the_dispatch_ended_from_the_dispatch_itself() {
        let tmp = TempDir::new().unwrap();
        let loaded = make_loaded(spec_with_prompt("hi"), tmp.path().to_path_buf());
        let profile = darkmux_types::Profile::default();
        let session = SessionId::adhoc(RunId::lab("run1").unwrap(), "coder", "w");
        let cases = [
            (1, r#"{"result":"escalation_compaction_reread_loop"}"#, false, Some("escalation_compaction_reread_loop"), false),
            (1, r#"{"result":"error"}"#, false, None, true),
            (0, r#"{"result":"stop"}"#, true, None, false),
        ];
        for (i, (exit_code, stdout, ok, escalation, errored)) in cases.into_iter().enumerate() {
            let run_dir = tmp.path().join(format!("run{i}"));
            fs::create_dir_all(&run_dir).unwrap();
            let d = Dispatched { exit_code, stdout: stdout.into(), stderr: "boom".into() };
            let r = finish_run(&FinishInputs { loaded: &loaded, run_dir: &run_dir, profile: &profile, profile_name: "p", session_id: &session, duration_ms: 1, reply_cap: None, n_ctx: None, prompt_tokens_counted: None, refused: &[] }, &d).unwrap();
            assert_eq!(r.ok, ok, "case {i}");
            assert_eq!(r.escalation.as_deref(), escalation, "case {i}");
            assert_eq!(r.error.is_some(), errored, "case {i}: an escalation is not an error");
            let manifest: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(run_dir.join("manifest.json")).unwrap()).unwrap();
            assert_eq!(manifest["ok"], ok, "case {i}");
            assert_eq!(manifest["manifest_schema_version"], darkmux_types::data_version::RUN_MANIFEST_SCHEMA_VERSION, "case {i}");
            assert_eq!(manifest.get("escalation").and_then(|e| e.as_str()), escalation, "case {i}");
        }
    }

    #[test]
    fn inspect_names_the_escalation_the_providers_own_manifest_recorded() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run1");
        fs::create_dir_all(&run_dir).unwrap();
        let loaded = make_loaded(spec_with_prompt("hi"), tmp.path().to_path_buf());
        let session = SessionId::adhoc(RunId::lab("run1").unwrap(), "coder", "w");
        for (end, want) in [
            (
                DispatchEnd::Escalated { reason: "escalation_compaction_reread_loop".into() },
                Some("outcome=escalated (escalation_compaction_reread_loop)"),
            ),
            (DispatchEnd::Failed, None),
        ] {
            write_manifest(
                &run_dir,
                &ManifestInputs {
                    workload_id: "w",
                    profile_name: "p",
                    profile_description: "",
                    duration_ms: 1,
                    session_id: &session,
                    verify: None,
                    end: &end,
                    refused: &[],
                },
            )
            .unwrap();
            let notes = PromptProvider.inspect(&loaded, &run_dir).unwrap().notes;
            let found = notes.iter().find(|n| n.starts_with("outcome=")).map(String::as_str);
            assert_eq!(found, want, "{notes:?}");
        }
    }

    fn spec_expecting(expect: serde_json::Value) -> WorkloadSpec {
        let mut spec = spec_with_prompt("x");
        spec.verify = Some(VerifySpec { expect: Some(expect.as_object().unwrap().clone()), ..Default::default() });
        spec
    }

    /// (`verify.expect`) The structured-output check decides the verdict and
    /// names the fields count in the details.
    #[test]
    fn run_verify_grades_the_reply_against_verify_expect() {
        let tmp = TempDir::new().unwrap();
        let loaded = make_loaded(spec_expecting(serde_json::json!({"a": "1", "b": "2"})), tmp.path().to_path_buf());
        let pass = run_verify(&loaded, r#"{"a": "1", "b": "2"}"#).unwrap();
        assert_eq!((pass.passed, pass.details.as_str()), (true, "fields 2/2: all match"));
        let fail = run_verify(&loaded, r#"{"a": "1", "b": "B"}"#).unwrap();
        assert_eq!((fail.passed, fail.details.as_str()), (false, "fields 1/2: missed b"));
    }

    /// Keywords beside `expect`: both must pass.
    #[test]
    fn run_verify_with_expect_and_keywords_needs_both() {
        let tmp = TempDir::new().unwrap();
        let mut spec = spec_expecting(serde_json::json!({"a": "1"}));
        spec.verify.as_mut().unwrap().must_not_contain = vec!["sorry".into()];
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        assert!(run_verify(&loaded, r#"{"a": "1"}"#).unwrap().passed);
        let v = run_verify(&loaded, r#"sorry {"a": "1"}"#).unwrap();
        assert!(!v.passed);
        assert_eq!(v.details, "fields 1/1: all match; disallowed keywords found: sorry");
    }

    /// A workload without `expect` verifies exactly as before.
    #[test]
    fn run_verify_without_expect_is_the_keyword_check_unchanged() {
        let tmp = TempDir::new().unwrap();
        let mut spec = spec_with_prompt("x");
        spec.verify = Some(VerifySpec::default());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let v = run_verify(&loaded, "anything").unwrap();
        assert_eq!((v.passed, v.details.as_str()), (true, "all keyword checks passed"));
        assert!(expect_report(&loaded, "anything").is_none());
    }

    /// The run's manifest carries the fields count in its verify object and
    /// the reply cap the dispatch sent; a run with neither carries neither key.
    #[test]
    fn finish_run_records_the_fields_count_and_the_reply_cap() {
        let tmp = TempDir::new().unwrap();
        let profile = darkmux_types::Profile::default();
        let session = SessionId::adhoc(RunId::lab("run1").unwrap(), "coder", "w");
        let envelope = |reply: &str| serde_json::json!({"result": "stop", "final_assistant": reply}).to_string();
        let manifest_of = |dir: &Path| -> serde_json::Value {
            serde_json::from_str(&fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap()
        };

        let loaded = make_loaded(spec_expecting(serde_json::json!({"a": "1", "b": "2", "c": "3"})), tmp.path().to_path_buf());
        let run_dir = tmp.path().join("structured");
        fs::create_dir_all(&run_dir).unwrap();
        let d = Dispatched { exit_code: 0, stdout: envelope(r#"{"a": "1", "b": "x", "c": "3"}"#), stderr: String::new() };
        let inputs = FinishInputs {
            loaded: &loaded,
            run_dir: &run_dir,
            profile: &profile,
            profile_name: "p",
            session_id: &session,
            duration_ms: 1,
            reply_cap: Some(30_000),
            n_ctx: Some(16_384),
            prompt_tokens_counted: Some(6_148),
            refused: &[],
        };
        let r = finish_run(&inputs, &d).unwrap();
        assert!(!r.verify.unwrap().passed);
        let m = manifest_of(&run_dir);
        assert_eq!(m["verify"]["passed"], false);
        assert_eq!(m["verify"]["fields_correct"], 2);
        assert_eq!(m["verify"]["fields_expected"], 3);
        assert_eq!(m["verify"]["fields_missed"], serde_json::json!(["b"]));
        assert_eq!(m["max_completion_tokens"], 30_000);
        assert_eq!(m["n_ctx"], 16_384, "the window the dispatch selected");
        assert_eq!(m["prompt_tokens_counted"], 6_148, "the tokens counted against that window");

        let plain = make_loaded(spec_with_prompt("x"), tmp.path().to_path_buf());
        let run_dir = tmp.path().join("plain");
        fs::create_dir_all(&run_dir).unwrap();
        let d = Dispatched { exit_code: 0, stdout: envelope("hi"), stderr: String::new() };
        let inputs = FinishInputs { loaded: &plain, run_dir: &run_dir, profile: &profile, profile_name: "p", session_id: &session, duration_ms: 1, reply_cap: None, n_ctx: None, prompt_tokens_counted: None, refused: &[] };
        finish_run(&inputs, &d).unwrap();
        let m = manifest_of(&run_dir);
        assert!(m.get("max_completion_tokens").is_none());
        assert!(m.get("n_ctx").is_none() && m.get("prompt_tokens_counted").is_none());
        assert!(m["verify"].is_null(), "no verify spec: not checked, as before");
    }

    /// The verdict calls an unclosed object "cut off" only when the run's
    /// trajectory says its last model call finished on `length`. A reply
    /// that stopped on its own and left the object open says so instead.
    #[test]
    fn finish_run_names_the_length_limit_only_when_the_reply_hit_it() {
        let tmp = TempDir::new().unwrap();
        let profile = darkmux_types::Profile::default();
        let session = SessionId::adhoc(RunId::lab("run1").unwrap(), "coder", "w");
        let loaded = make_loaded(spec_expecting(serde_json::json!({"a": "1"})), tmp.path().to_path_buf());
        let stdout = serde_json::json!({"result": "stop", "final_assistant": r#"{"a": "#}).to_string();
        for (finish, want) in [
            ("length", "fields 0/1: the reply's JSON object is cut off: the reply hit its length limit"),
            ("stop", "fields 0/1: the reply's JSON object ends before it closes"),
            ("", "fields 0/1: the reply's JSON object ends before it closes"),
        ] {
            let run_dir = tmp.path().join(format!("run-{finish}"));
            fs::create_dir_all(&run_dir).unwrap();
            fs::write(
                run_dir.join("trajectory.jsonl"),
                format!("{{\"type\":\"model.completed\",\"seq\":1,\"ts\":1,\"finish_reason\":\"{finish}\"}}\n"),
            )
            .unwrap();
            let d = Dispatched { exit_code: 0, stdout: stdout.clone(), stderr: String::new() };
            let inputs = FinishInputs { loaded: &loaded, run_dir: &run_dir, profile: &profile, profile_name: "p", session_id: &session, duration_ms: 1, reply_cap: None, n_ctx: None, prompt_tokens_counted: None, refused: &[] };
            let r = finish_run(&inputs, &d).unwrap();
            assert_eq!(r.verify.unwrap().details, want, "finish_reason {finish:?}");
            let inspected = PromptProvider.inspect(&loaded, &run_dir).unwrap();
            assert_eq!(inspected.verify.unwrap().details, want, "inspect, finish_reason {finish:?}");
        }
    }

    /// `replyRoom`: the window minus one token per two prompt bytes and the
    /// fixed overhead. A window the prompt nearly fills is refused, not
    /// dispatched.
    #[test]
    fn reply_room_is_the_window_minus_one_token_per_two_bytes() {
        assert_eq!(reply_room_from(32_768, 3_000, "w").unwrap(), 32_768 - 1_500 - REPLY_ROOM_OVERHEAD_TOKENS);
        assert_eq!(reply_room_from(8_192, 1, "w").unwrap(), 8_192 - 1 - REPLY_ROOM_OVERHEAD_TOKENS);
        // A small model's 16K window must still run a ~23 KB extraction
        // prompt (role, preamble and page). That one measured 6,144 tokens
        // on a 1.7B model; one token per byte refused it outright.
        assert!(reply_room_from(16_384, 22_890, "w").unwrap() >= 4_000);
        let err = reply_room_from(4_096, 11_000, "w").unwrap_err().to_string();
        assert!(err.contains("raise the profile's n_ctx"), "{err}");
    }

    /// The room counts the dispatch's own system prompt and the message in
    /// bytes, needs the selected model's n_ctx, and is absent on a route that
    /// is not one call.
    #[test]
    fn reply_room_follows_the_dispatchs_own_route() {
        use darkmux_crew::dispatch::OneCallContext;
        assert_eq!(reply_room_for(None, "x", "w").unwrap(), None, "the container path sends no cap");
        let ctx = |n_ctx| Some(OneCallContext { model: "m".into(), n_ctx, max_output_tokens: None, system_prompt_bytes: 5_700, system_prompt_chars: 5_700 });
        let err = reply_room_for(ctx(None), "x", "w").unwrap_err().to_string();
        assert!(err.contains("`m` declares no n_ctx"), "{err}");
        // "日本" is six bytes, counted with the system prompt's 5,700: 5,706
        // bytes, 2,853 tokens.
        assert_eq!(reply_room_for(ctx(Some(16_384)), "日本", "w").unwrap(), Some(16_384 - 2_853 - REPLY_ROOM_OVERHEAD_TOKENS));
    }

    /// A model whose output limit is below what its window leaves gets its
    /// output limit (a hosted Opus: 200K window, 128K output); one whose
    /// limit is above it gets the window's remainder.
    #[test]
    fn reply_room_stops_at_the_models_output_limit() {
        use darkmux_crew::dispatch::OneCallContext;
        let ctx = |max| Some(OneCallContext { model: "m".into(), n_ctx: Some(200_000), max_output_tokens: max, system_prompt_bytes: 5_700, system_prompt_chars: 5_700 });
        assert_eq!(reply_room_for(ctx(Some(128_000)), "x", "w").unwrap(), Some(128_000));
        let room = 200_000 - 2_851 - REPLY_ROOM_OVERHEAD_TOKENS;
        assert_eq!(reply_room_for(ctx(Some(250_000)), "x", "w").unwrap(), Some(room));
        assert_eq!(reply_room_for(ctx(None), "x", "w").unwrap(), Some(room));
        let err = reply_room_for(ctx(Some(0)), "x", "w").unwrap_err().to_string();
        assert!(err.contains("declares max_output_tokens 0"), "{err}");
    }

    /// A one-call run writes the trajectory of its one call, from the counts
    /// the call itself reported, so `run stats` and `lab submit` can read it.
    #[test]
    fn a_one_call_run_writes_the_trajectory_of_its_one_call() {
        let tmp = TempDir::new().unwrap();
        let stdout = serde_json::json!({"result": "stop", "final_assistant": "{}", "metrics": {
            "model": "m", "endpoint": "e", "wall_ms": 13_100, "turns": 1,
            "prompt_tokens": 6_000, "completion_tokens": 900, "total_tokens": 6_900}})
        .to_string();
        write_one_call_trajectory(&stdout, 5_700, 21_000, tmp.path(), 1_000_000);
        let fold = crate::lab::inspect::checked_run_trajectory(tmp.path()).unwrap();
        assert_eq!(fold.events, 5, "the header is a marker, not an event");
        assert_eq!(fold.schema_version.as_deref(), Some(darkmux_trajectory::TRAJECTORY_SCHEMA_VERSION));
        assert_eq!(fold.model_calls, 1);
        assert_eq!(fold.frames, vec![Some(900)]);
        let start = fold.start.as_ref().unwrap();
        assert_eq!((start.model.as_str(), start.system_chars, start.prompt_chars, start.ts), ("m", 5_700, 21_000, 1_000_000 - 13_100));
        let complete = fold.complete.as_ref().unwrap();
        assert_eq!((complete.result.as_str(), complete.wall_ms), ("stop", 13_100));
        let stats = crate::lab::stats::compute_from_dir(tmp.path(), &tmp.path().join("flows")).expect("the run's numbers can be read");
        // The call is one stream, start to end, so the run has a rate and
        // reconciles: no rate means `lab submit` uploads tokensPerSecond null.
        assert_eq!(fold.streams.len(), 1);
        assert_eq!(fold.streams[0].ms(), 13_100);
        assert_eq!(stats.tok_per_s, Some(68.7));
        assert!(stats.checks.frames_match_streams && stats.checks.all_streams_billed);
        assert!(!stats.unreconciled().iter().any(|w| w.contains("streams")), "{:?}", stats.unreconciled());
        // The call's finish reason, when the envelope carries it, is the
        // trajectory's: the grade reads it to tell a cut-off reply from one
        // that stopped on its own.
        let cut = TempDir::new().unwrap();
        let stdout = serde_json::json!({"result": "stop", "final_assistant": "{", "metrics": {
            "model": "m", "endpoint": "e", "wall_ms": 100, "turns": 1, "finish_reason": "length",
            "prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}})
        .to_string();
        write_one_call_trajectory(&stdout, 1, 1, cut.path(), 1_000_000);
        assert!(hit_length_limit(cut.path()));
        let fold = crate::lab::inspect::checked_run_trajectory(cut.path()).unwrap();
        assert_eq!(fold.model_calls, 1);
        crate::lab::stats::compute_from_dir(cut.path(), &cut.path().join("flows")).expect("a cut call's numbers read too");
        assert!(!hit_length_limit(tmp.path()), "no finish reason is not a hit");
        // A reply that is not the one-call envelope leaves no file to misread.
        let other = TempDir::new().unwrap();
        write_one_call_trajectory("plain text", 1, 1, other.path(), 1_000_000);
        assert!(!other.path().join("trajectory.jsonl").exists());
    }

    /// (#3367) A hosted stream that reported when its first text arrived gets
    /// a first-token mark, and a second output chunk so decode speed is the
    /// tokens after that mark. A call that names neither stays unmarked.
    #[test]
    fn a_one_call_run_marks_time_to_first_token_when_the_stream_reported_it() {
        let tmp = TempDir::new().unwrap();
        let stdout = serde_json::json!({"result": "stop", "final_assistant": "hello", "metrics": {
            "model": "m", "endpoint": "e", "wall_ms": 13_100, "turns": 1,
            "prompt_tokens": 6_000, "completion_tokens": 900, "total_tokens": 6_900,
            "first_token_ms": 2_000, "output_chunks": 4}})
        .to_string();
        write_one_call_trajectory(&stdout, 100, 100, tmp.path(), 1_000_000);
        let fold = crate::lab::inspect::checked_run_trajectory(tmp.path()).unwrap();
        assert_eq!(fold.streams[0].output_chunks, 2, "two observations: the first text, and the rest");
        assert_eq!(fold.streams[0].first_output_after_ms(), Some(2_000));
        let stats = crate::lab::stats::compute_from_dir(tmp.path(), &tmp.path().join("flows")).expect("numbers");
        assert_eq!(stats.first_token_ms_median, Some(2_000));
        assert_eq!(stats.first_token_ms_max, Some(2_000));
        // 900 completion tokens over the 11.1 s after the first token.
        assert_eq!(stats.decode_tok_per_s, Some(81.1));

        let one = TempDir::new().unwrap();
        let stdout = serde_json::json!({"result": "stop", "final_assistant": "hello", "metrics": {
            "model": "m", "endpoint": "e", "wall_ms": 13_100, "turns": 1,
            "prompt_tokens": 6_000, "completion_tokens": 900, "total_tokens": 6_900,
            "first_token_ms": 2_000, "output_chunks": 1}})
        .to_string();
        write_one_call_trajectory(&stdout, 100, 100, one.path(), 1_000_000);
        let stats = crate::lab::stats::compute_from_dir(one.path(), &one.path().join("flows")).expect("numbers");
        assert_eq!(stats.first_token_ms_median, Some(2_000), "one chunk still has a wait");
        assert_eq!(stats.decode_tok_per_s, None, "one chunk has no decode interval");

        let late = TempDir::new().unwrap();
        let stdout = serde_json::json!({"result": "stop", "final_assistant": "hello", "metrics": {
            "model": "m", "endpoint": "e", "wall_ms": 13_100, "turns": 1,
            "prompt_tokens": 6_000, "completion_tokens": 900, "total_tokens": 6_900,
            "first_token_ms": 13_100, "output_chunks": 4}})
        .to_string();
        write_one_call_trajectory(&stdout, 100, 100, late.path(), 1_000_000);
        let stats = crate::lab::stats::compute_from_dir(late.path(), &late.path().join("flows")).expect("numbers");
        assert_eq!(stats.first_token_ms_median, None, "text that arrives as the call ends is not a measured wait");
        assert_eq!(stats.decode_tok_per_s, None);
    }

    /// The runtime's trajectory lands in the run dir, where `run stats` and
    /// `lab submit` read a run's numbers; no out dir copies nothing.
    #[test]
    fn the_runtime_trajectory_is_kept_in_the_run_dir() {
        let tmp = TempDir::new().unwrap();
        let (out, run_dir) = (tmp.path().join("out"), tmp.path().join("run"));
        fs::create_dir_all(out.join(".darkmux-runtime")).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(out.join(".darkmux-runtime/trajectory.jsonl"), "{\"event\":\"x\"}\n").unwrap();
        preserve_trajectory(None, &run_dir);
        assert!(!run_dir.join("trajectory.jsonl").exists());
        preserve_trajectory(Some(&out), &run_dir);
        assert_eq!(fs::read_to_string(run_dir.join("trajectory.jsonl")).unwrap(), "{\"event\":\"x\"}\n");
    }

    /// The named out dir lives in the process temp directory, not under the
    /// test's `TempDir`, so it has to be removed explicitly. Its name is the
    /// role and the clock's microseconds, so each test arms a role of its own:
    /// two tests running in parallel could otherwise name the same dir.
    struct RemoveOut(std::path::PathBuf);
    impl Drop for RemoveOut {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A dispatch that returns `Err` never reaches the success-path copy.
    /// The guard is armed with the named out dir before the call, and its
    /// Drop is what keeps the trajectory and closes it.
    #[test]
    fn an_interrupted_prompt_dispatch_keeps_and_closes_its_trajectory() {
        let tmp = TempDir::new().unwrap();
        let run = tmp.path().join("run");
        fs::create_dir_all(&run).unwrap();
        let session = SessionId::adhoc(RunId::lab("run1").unwrap(), "reviewer", "w");
        let mut opts = dispatch_opts("reviewer", "hi", &session, None, None, "p", darkmux_crew::dispatch::LabInputs::Builtin);
        let guard = PromptDispatchGuard::arm("reviewer-a", "w", &run, &mut opts);
        let out = opts.host_out.clone().expect("the out dir is named before dispatch");
        let _remove_out = RemoveOut(out.clone());
        fs::create_dir_all(out.join(".darkmux-runtime")).unwrap();
        fs::write(
            out.join(".darkmux-runtime/trajectory.jsonl"),
            "{\"type\":\"dispatch.start\",\"ts\":1000,\"model\":\"m\"}\n\
             {\"type\":\"model.completed\",\"seq\":1,\"ts\":2000,\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"total_tokens\":6}}\n",
        )
        .unwrap();
        drop(guard);
        let fold = darkmux_trajectory::TrajectoryFold::from_path(&run.join("trajectory.jsonl"));
        assert_eq!(fold.turns(), 1, "what completed is in the run dir");
        assert_eq!(fold.complete.expect("a terminal record").result, darkmux_trajectory::RESULT_INTERRUPTED);
        assert!(!run.join("manifest.json").exists(), "a clean copy writes no refusal manifest");
    }

    /// A dispatch that returns `Err` never reaches `finish_run`, and the lab
    /// writes no manifest for that outcome. A refused copy still has to land
    /// on one: the warning used to say it had, while Drop threw the list away.
    #[test]
    fn a_dropped_prompt_guard_records_a_refused_copy_on_the_manifest() {
        let tmp = TempDir::new().unwrap();
        let run = tmp.path().join("run");
        fs::create_dir_all(&run).unwrap();
        let session = SessionId::adhoc(RunId::lab("run1").unwrap(), "reviewer", "w");
        let mut opts = dispatch_opts("reviewer", "hi", &session, None, None, "p", darkmux_crew::dispatch::LabInputs::Builtin);
        let guard = PromptDispatchGuard::arm("reviewer-b", "w", &run, &mut opts);
        let out = opts.host_out.clone().unwrap();
        let _remove_out = RemoveOut(out.clone());
        fs::create_dir_all(out.join(".darkmux-runtime")).unwrap();
        fs::write(out.join(".darkmux-runtime/trajectory.jsonl"), "{\"type\":\"dispatch.start\",\"ts\":1,\"model\":\"m\"}\n").unwrap();
        let secret = tmp.path().join("host-secret.txt");
        fs::write(&secret, "HOST-SECRET-2869").unwrap();
        std::os::unix::fs::symlink(&secret, out.join(".darkmux-runtime/findings.jsonl")).unwrap();

        drop(guard);

        assert!(!run.join("findings.jsonl").exists(), "a refused symlink must not be copied");
        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(run.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["provider"], "prompt");
        // `lab inspect` reads a manifest by its workload; without one it errors.
        assert_eq!(manifest["workload"], "w");
        assert_eq!(manifest["ok"], false, "the dispatch did not complete");
        assert_eq!(manifest["schema_version"], 6);
        assert_eq!(manifest["refused_artifacts"][0]["file"], "findings.jsonl");
        assert!(manifest["refused_artifacts"][0]["reason"].as_str().unwrap().contains("symlink"));
        let copied = fs::read_to_string(run.join("trajectory.jsonl")).unwrap();
        assert!(copied.contains("interrupted"), "the error path still closes the trajectory");
        assert!(!copied.contains("HOST-SECRET-2869"));
    }

    /// The success path copies, records a refusal, and disarms. Disarming
    /// matters: the guard must not then mark the copied trajectory interrupted.
    #[test]
    fn a_kept_prompt_dispatch_records_a_refusal_and_does_not_close_the_trajectory() {
        let tmp = TempDir::new().unwrap();
        let run = tmp.path().join("run");
        fs::create_dir_all(&run).unwrap();
        let session = SessionId::adhoc(RunId::lab("run1").unwrap(), "reviewer", "w");
        let mut opts = dispatch_opts("reviewer", "hi", &session, None, None, "p", darkmux_crew::dispatch::LabInputs::Builtin);
        let guard = PromptDispatchGuard::arm("reviewer-c", "w", &run, &mut opts);
        let out = opts.host_out.clone().unwrap();
        let _remove_out = RemoveOut(out.clone());
        fs::create_dir_all(out.join(".darkmux-runtime")).unwrap();
        // Open on purpose. The success path copies it and must not close it;
        // closing is the error path's job, and only when the runtime didn't.
        let body = "{\"type\":\"dispatch.start\",\"ts\":1,\"model\":\"m\"}\n";
        fs::write(out.join(".darkmux-runtime/trajectory.jsonl"), body).unwrap();
        let secret = tmp.path().join("host-secret.txt");
        fs::write(&secret, "HOST-SECRET-2869").unwrap();
        std::os::unix::fs::symlink(&secret, out.join(".darkmux-runtime/findings.jsonl")).unwrap();

        let refused = guard.keep(Some(&out), &run);
        let copied = fs::read_to_string(run.join("trajectory.jsonl")).unwrap();
        assert_eq!(copied, body, "a disarmed guard must not append interrupted");
        assert!(!copied.contains("interrupted"));
        assert!(!run.join("findings.jsonl").exists(), "a refused symlink must not be copied");
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].file, "findings.jsonl");
        assert!(refused[0].reason.contains("symlink"), "{}", refused[0].reason);

        let loaded = make_loaded(spec_with_prompt("x"), tmp.path().to_path_buf());
        let profile = darkmux_types::Profile::default();
        let d = Dispatched { exit_code: 0, stdout: r#"{"result":"stop","final_assistant":"hi"}"#.into(), stderr: String::new() };
        finish_run(
            &FinishInputs {
                loaded: &loaded,
                run_dir: &run,
                profile: &profile,
                profile_name: "p",
                session_id: &session,
                duration_ms: 1,
                reply_cap: None,
                n_ctx: None,
                prompt_tokens_counted: None,
                refused: &refused,
            },
            &d,
        )
        .unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&fs::read_to_string(run.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["refused_artifacts"][0]["file"], "findings.jsonl");
        assert!(manifest["refused_artifacts"][0]["reason"].as_str().unwrap().contains("symlink"));
        assert_eq!(fs::read_to_string(run.join("trajectory.jsonl")).unwrap(), body);
    }

    /// A workload that binds a fixture reads its promptFile from the run's
    /// copy of that fixture; one that does not, from beside the document.
    #[test]
    fn prompt_file_resolves_in_the_fixture_copy_when_the_workload_binds_one() {
        let tmp = TempDir::new().unwrap();
        let (doc_dir, sandbox) = (tmp.path().join("doc"), tmp.path().join("sandbox"));
        fs::create_dir_all(&doc_dir).unwrap();
        fs::create_dir_all(&sandbox).unwrap();
        fs::write(doc_dir.join("in.txt"), "beside the document").unwrap();
        fs::write(sandbox.join("in.txt"), "from the fixture").unwrap();
        let mut spec = spec_with_prompt("");
        spec.prompt = None;
        spec.prompt_file = Some("in.txt".into());
        let unbound = make_loaded(spec.clone(), doc_dir.clone());
        assert_eq!(resolve_prompt_from(&unbound, prompt_base(&unbound, &sandbox)).unwrap(), "beside the document");
        spec.requires_fixture = Some("biz@1.0".into());
        let bound = make_loaded(spec, doc_dir);
        assert_eq!(resolve_prompt_from(&bound, prompt_base(&bound, &sandbox)).unwrap(), "from the fixture");
    }

    #[test]
    fn provider_metadata() {
        let p = PromptProvider;
        assert_eq!(p.id(), "prompt");
    }

    #[test]
    fn resolve_prompt_inline() {
        let tmp = TempDir::new().unwrap();
        let loaded = make_loaded(spec_with_prompt("hi there"), tmp.path().to_path_buf());
        assert_eq!(resolve_prompt(&loaded).unwrap(), "hi there");
    }

    #[test]
    fn resolve_prompt_from_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("p.txt"), "from-file").unwrap();
        let mut spec = spec_with_prompt("");
        spec.prompt = None;
        spec.prompt_file = Some("p.txt".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        assert_eq!(resolve_prompt(&loaded).unwrap(), "from-file");
    }

    #[test]
    fn resolve_prompt_missing_errors() {
        let tmp = TempDir::new().unwrap();
        let mut spec = spec_with_prompt("");
        spec.prompt = None;
        spec.prompt_file = None;
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let err = resolve_prompt(&loaded).unwrap_err();
        assert!(err.to_string().contains("must define prompt"));
    }

    #[test]
    fn pick_role_from_manifest() {
        let tmp = TempDir::new().unwrap();
        let mut spec = spec_with_prompt("x");
        spec.role = Some("analyst".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        assert_eq!(pick_role(&loaded), "analyst");
    }

    // `DARKMUX_DEFAULT_ROLE` is a process-global; this test removes it, so it
    // needs the same `#[serial_test::serial]` its sibling twin
    // (`coding_task::tests::pick_role_default_coder`) already carries for the
    // identical mutation, or a concurrent unannotated test reading the var
    // could observe it cleared mid-run. Unguarded, pre-existing (found
    // 2026-09 auditing #2590's fixes for the same class of race).
    #[serial_test::serial]
    #[test]
    fn pick_role_default_code_reviewer() {
        let tmp = TempDir::new().unwrap();
        let loaded = make_loaded(spec_with_prompt("x"), tmp.path().to_path_buf());
        unsafe { env::remove_var("DARKMUX_DEFAULT_ROLE") };
        assert_eq!(pick_role(&loaded), "code-reviewer");
    }

    #[test]
    fn extract_reply_handles_internal_runtime_envelope() {
        // Phase-A's darkmux-runtime --json envelope.
        let json = r#"{"result":"stop","final_assistant":"hello from internal runtime","metrics":{"wall_ms":2135}}"#;
        assert_eq!(extract_reply_text(json), "hello from internal runtime");
    }

    #[test]
    fn extract_reply_handles_top_level_reply() {
        let json = r#"{"reply":"plain reply"}"#;
        assert_eq!(extract_reply_text(json), "plain reply");
    }

    #[test]
    fn extract_reply_returns_raw_when_unparseable() {
        assert_eq!(extract_reply_text("not json"), "not json");
    }

    #[test]
    fn extract_reply_empty_input() {
        assert_eq!(extract_reply_text(""), "");
        assert_eq!(extract_reply_text("   "), "");
    }

    #[test]
    fn extract_reply_unknown_shape_returns_empty() {
        let json = r#"{"unrelated":"value"}"#;
        assert_eq!(extract_reply_text(json), "");
    }

    #[test]
    fn run_verify_checks_nothing_without_a_spec() {
        let tmp = TempDir::new().unwrap();
        let loaded = make_loaded(spec_with_prompt("x"), tmp.path().to_path_buf());
        assert!(run_verify(&loaded, "anything").is_none());
    }

    /// Every verify state renders its own note; "nothing checked" never
    /// reads as ok.
    #[test]
    fn verify_note_names_each_state() {
        let v = |passed| VerifyOutcome { passed, details: "d".into() };
        assert_eq!(verify_note(Some(&v(true))), "verify: ok: d");
        assert_eq!(verify_note(Some(&v(false))), "verify: fail: d");
        assert_eq!(verify_note(None), "verify: not checked: no verify spec");
    }

    #[test]
    fn run_verify_passes_when_keywords_present() {
        let tmp = TempDir::new().unwrap();
        let mut spec = spec_with_prompt("x");
        spec.verify = Some(VerifySpec {
            must_contain: vec!["alpha".into(), "beta".into()],
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let v = run_verify(&loaded, "we have alpha and beta here").unwrap();
        assert!(v.passed);
    }

    /// (#2493 follow-up) NOT a claim about `quick-q`'s shipped manifest —
    /// that keyword is back to the bare "active" (see
    /// `quick_q_verify_keyword_is_the_bare_word_not_a_widened_stem` in
    /// `workloads/load.rs`). This is a characterization of the matcher
    /// itself for a `command`-less spec: a stem is still ordinary substring
    /// matching, so a workload that deliberately WANTS one (unlike
    /// `quick-q`) still gets it. It is not a guard by itself — every reply
    /// here is already lowercase and already contains "activ" literally,
    /// so it stays green under the pre-#2493 matcher too; see
    /// `run_verify_bare_keyword_rejects_wrong_answers_using_other_inflections`
    /// below for the case a stem actually gets wrong.
    #[test]
    fn run_verify_stem_matching_still_works_for_a_spec_that_chooses_one() {
        let tmp = TempDir::new().unwrap();
        let mut spec = spec_with_prompt("x");
        spec.verify = Some(VerifySpec {
            must_contain: vec!["activ".into()],
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        for reply in [
            "it activates a small fraction of its parameters",
            "the activated subset stays small",
            "this is a form of sparse activation",
            "only a few parameters are active per forward pass",
        ] {
            let v = run_verify(&loaded, reply).unwrap();
            assert!(v.passed, "expected {reply:?} to pass a stemmed \"activ\" check, got {v:?}");
        }
    }

    /// (#2493 follow-up) A bare, un-stemmed keyword — the shape `quick-q`
    /// actually ships — REJECTS three wrong answers a widened `"activ"`
    /// stem let through: a different mechanism entirely, the wrong
    /// direction, and an explicit "no difference" (the exact negation of a
    /// correct answer). None of the three contains the literal word
    /// "active" as a substring — each uses a different inflection
    /// ("activation", "activates", "activate") — so the bare keyword
    /// correctly fails all three rather than passing them on the strength
    /// of an unrelated inflected word appearing somewhere in the reply.
    #[test]
    fn run_verify_bare_keyword_rejects_wrong_answers_using_other_inflections() {
        let tmp = TempDir::new().unwrap();
        let mut spec = spec_with_prompt("x");
        spec.verify = Some(VerifySpec {
            must_contain: vec!["active".into()],
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let wrong_answers = [
            // Names a different mechanism entirely (quantization, not
            // active-parameter count).
            "The difference comes from weight quantization, not parameter activation.",
            // Describes the wrong direction (reverses which architecture
            // activates fewer parameters).
            "The dense model activates fewer parameters per token than the MoE model.",
            // Asserts there is no difference at all — the exact negation
            // of a correct answer.
            "There is no observable difference between the two on Apple Silicon; both architectures activate the same number of parameters.",
        ];
        for reply in wrong_answers {
            let v = run_verify(&loaded, reply).unwrap();
            assert!(!v.passed, "expected a wrong answer to be REJECTED: {reply:?}, got {v:?}");
        }
    }

    /// (#2493 follow-up) Case must not matter for a free-text, `command`-
    /// less keyword check — a model is free to capitalize a word at a
    /// sentence boundary the prompt itself never did, and that is not
    /// evidence the answer missed the concept the check is guarding.
    #[test]
    fn run_verify_keyword_match_is_case_insensitive_without_a_command() {
        let tmp = TempDir::new().unwrap();
        let mut spec = spec_with_prompt("x");
        spec.verify = Some(VerifySpec {
            must_contain: vec!["active".into()],
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let v = run_verify(&loaded, "Active parameters differ between the two.").unwrap();
        assert!(v.passed, "expected a capitalized match to pass, got {v:?}");
    }

    /// (#2493 follow-up, MUST FIX 2) A workload that ALSO carries a
    /// `command` (demo-quickstart's, quick-coding's, medium-coding's
    /// shape) asks the model to quote a deterministic test runner's own
    /// output verbatim — that keyword's casing is fixed by the runner, so
    /// matching stays case-SENSITIVE there. Without this, a two-letter
    /// keyword like "OK" lowercases into a substring probe that any prose
    /// mentioning "looked" or "book" satisfies by accident — a false PASS
    /// on a reply that plainly says the suite was never run.
    #[test]
    fn run_verify_command_spec_keyword_match_stays_case_sensitive() {
        let tmp = TempDir::new().unwrap();
        let mut spec = spec_with_prompt("x");
        spec.verify = Some(VerifySpec {
            command: Some("true".into()),
            must_contain: vec!["OK".into()],
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let v = run_verify(&loaded, "I looked at the test file but never ran the suite.").unwrap();
        assert!(
            !v.passed,
            "a lowercase 'ok' inside 'looked' must not satisfy a command-spec's must_contain: {v:?}"
        );
    }

    /// (#2493 follow-up, MUST FIX 2) The other direction of the same gap:
    /// a `command`-spec's negative keyword ("FAILED") must not
    /// case-insensitively match a CORRECT reply that narrates PAST failure
    /// in prose ("failed before my fix, passes now") — that reply is
    /// reporting a fix, not a current failure, and a case-insensitive
    /// match would flip a passing run to a false failure.
    #[test]
    fn run_verify_command_spec_negative_keyword_does_not_false_fail_on_past_tense_prose() {
        let tmp = TempDir::new().unwrap();
        let mut spec = spec_with_prompt("x");
        spec.verify = Some(VerifySpec {
            command: Some("true".into()),
            must_not_contain: vec!["FAILED".into()],
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let v = run_verify(&loaded, "The suite failed before my fix; it passes now.").unwrap();
        assert!(
            v.passed,
            "lowercase 'failed' narrating past tense must not trip a command-spec's must_not_contain: {v:?}"
        );
    }

    #[test]
    fn run_verify_fails_when_required_missing() {
        let tmp = TempDir::new().unwrap();
        let mut spec = spec_with_prompt("x");
        spec.verify = Some(VerifySpec {
            must_contain: vec!["alpha".into(), "missing".into()],
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let v = run_verify(&loaded, "alpha here only").unwrap();
        assert!(!v.passed);
        assert!(v.details.contains("missing"));
    }

    #[test]
    fn run_verify_fails_when_disallowed_present() {
        let tmp = TempDir::new().unwrap();
        let mut spec = spec_with_prompt("x");
        spec.verify = Some(VerifySpec {
            must_not_contain: vec!["forbidden".into()],
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let v = run_verify(&loaded, "this contains forbidden text").unwrap();
        assert!(!v.passed);
        assert!(v.details.contains("disallowed"));
    }

    #[test]
    fn run_verify_fails_with_both_classes_of_error() {
        let tmp = TempDir::new().unwrap();
        let mut spec = spec_with_prompt("x");
        spec.verify = Some(VerifySpec {
            must_contain: vec!["alpha".into()],
            must_not_contain: vec!["bad".into()],
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let v = run_verify(&loaded, "this has bad words but no required marker").unwrap();
        assert!(!v.passed);
        assert!(v.details.contains("missing"));
        assert!(v.details.contains("disallowed"));
    }

    #[test]
    fn setup_creates_run_dir() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let loaded = make_loaded(spec_with_prompt("x"), tmp.path().to_path_buf());
        PromptProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .unwrap();
        assert!(run_dir.exists());
    }

    #[test]
    fn inspect_handles_missing_files_gracefully() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("empty-run");
        fs::create_dir_all(&run_dir).unwrap();
        let loaded = make_loaded(spec_with_prompt("x"), tmp.path().to_path_buf());
        let report = PromptProvider.inspect(&loaded, &run_dir).unwrap();
        assert_eq!(report.workload_id, "t");
        // run_id falls back to the run-dir basename when the manifest is missing.
        assert_eq!(report.run_id, "empty-run");
        assert_eq!(report.turns, 1);
        assert_eq!(report.compactions, 0);
    }

    /// Forward-compat: when the manifest carries a `run_id`, inspect returns
    /// that value rather than the dir basename. Locks the new schema (v2).
    #[test]
    fn inspect_uses_run_id_from_manifest() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("dir-name-differs");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("manifest.json"),
            r#"{"schema_version":2,"run_id":"the-canonical-id","workload":"t","provider":"prompt","profile":"deep","duration_ms":42,"ok":true,"session_id":"darkmux-prompt-t-1"}"#,
        )
        .unwrap();
        let loaded = make_loaded(spec_with_prompt("x"), tmp.path().to_path_buf());
        let report = PromptProvider.inspect(&loaded, &run_dir).unwrap();
        assert_eq!(report.run_id, "the-canonical-id");
        assert_eq!(report.workload_id, "t");
        assert_eq!(report.walltime_ms, 42);
    }

    /// The error path's manifest: nothing refused writes nothing; a refusal
    /// fills only what the manifest does not already say.
    #[test]
    fn a_refusal_manifest_fills_only_what_is_missing() {
        let tmp = TempDir::new().unwrap();
        let refused = [crate::providers::coding_task::RefusedArtifact { file: "trajectory.jsonl".into(), reason: "a symlink".into() }];
        let fresh = tmp.path().join("run-7");
        std::fs::create_dir_all(&fresh).unwrap();
        write_refusal_manifest(&fresh, "w", &[]).unwrap();
        assert!(!fresh.join("manifest.json").exists());
        write_refusal_manifest(&fresh, "w", &refused).unwrap();
        let m = RunManifest::read(&fresh).unwrap();
        assert_eq!((m.workload.as_deref(), m.provider.as_deref(), m.run_id.as_deref(), m.ok), (Some("w"), Some("prompt"), Some("run-7"), Some(false)));
        assert_eq!(m.schema_version, Some(6));
        assert_eq!(m.refused_artifacts.as_ref().map(Vec::len), Some(1));

        let kept = tmp.path().join("run-8");
        std::fs::create_dir_all(&kept).unwrap();
        std::fs::write(kept.join("manifest.json"), r#"{"workload": "x", "provider": "p", "run_id": "r", "ok": true, "schema_version": 8, "manifest_schema_version": "1.0"}"#).unwrap();
        write_refusal_manifest(&kept, "w", &refused).unwrap();
        let m = RunManifest::read(&kept).unwrap();
        assert_eq!((m.workload.as_deref(), m.provider.as_deref(), m.run_id.as_deref(), m.ok), (Some("x"), Some("p"), Some("r"), Some(true)));
        assert_eq!((m.schema_version, m.manifest_schema_version.as_deref()), (Some(8), Some("1.0")));
    }
}
