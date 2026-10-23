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
        let reply_room = reply_room_tokens(loaded, profile, &role, &prompt)?;
        let session_id = SessionId::adhoc(run.clone(), &role, &loaded.manifest.workload.id);
        // (#2511) Report the id back to the lab harness BEFORE dispatching —
        // this is the ONE mint for this run, so it is the run's own
        // governing dispatch session.
        on_session_id(&session_id);

        let started = std::time::Instant::now();
        let (dispatched, one_call) = dispatch_via_internal(
            &role,
            &prompt,
            &session_id,
            loaded.manifest.workload.image.as_deref(),
            config_path,
            profile_name,
            reply_room,
        )?;
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
                reply_cap: reply_room.filter(|_| one_call),
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
        let verify_outcome = run_verify(loaded, &reply);
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
    let verify = run_verify(f.loaded, &reply);
    let recorded_verify =
        verify.as_ref().map(|v| VerifyReport { passed: v.passed, details: v.details.clone() });
    let fields = expect_report(f.loaded, &reply);
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
        },
    )?;
    record_structured_output(f.run_dir, fields.as_ref(), f.reply_cap)?;
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
            extras: Default::default(),
        })))),
        ..RunManifest::default()
    };
    m.end.record_in(&mut manifest);
    manifest.write(run_dir)?;
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

/// Where a `promptFile` resolves: the run's own copy of the fixture when the
/// workload binds one (so the input is the one the recorded fixture commit
/// holds), else beside the workload document.
fn prompt_base<'a>(loaded: &'a LoadedWorkload, sandbox_dir: &'a Path) -> &'a Path {
    match loaded.manifest.workload.requires_fixture {
        Some(_) => sandbox_dir,
        None => &loaded.base_dir,
    }
}

/// Tokens of template and identity text a dispatch adds around the role's
/// prompt and the message, which the character count below cannot see.
const REPLY_ROOM_OVERHEAD_TOKENS: u32 = 512;
/// Below this the window is too small to be worth a dispatch.
const REPLY_ROOM_MIN_TOKENS: u32 = 256;

/// The reply room a `replyRoom: "context"` workload asks for: the profile
/// model's `n_ctx` minus the prompt. The host has no tokenizer, so the prompt
/// (the role's system prompt plus the message) is over-counted at one token
/// per three characters, plus a fixed overhead: the reply may get a little
/// less than the true remainder, never more than the window holds. `None`
/// when the workload does not ask.
fn reply_room_tokens(loaded: &LoadedWorkload, profile: &Profile, role: &str, prompt: &str) -> Result<Option<u32>> {
    let Some(ReplyRoom::Context) = loaded.manifest.workload.reply_room else { return Ok(None) };
    let model_id = profile.default_model_id().unwrap_or_default();
    let n_ctx = profile
        .models
        .iter()
        .find(|m| m.id == model_id)
        .and_then(|m| m.n_ctx)
        .ok_or_else(|| anyhow!("workload \"{}\" asks for replyRoom \"context\", but the profile's model `{model_id}` declares no n_ctx", loaded.manifest.workload.id))?;
    let system = darkmux_crew::loader::role_prompt(role).unwrap_or_default();
    Ok(Some(reply_room_from(n_ctx, system.chars().count() + prompt.chars().count(), &loaded.manifest.workload.id)?))
}

fn reply_room_from(n_ctx: u32, prompt_chars: usize, workload_id: &str) -> Result<u32> {
    let prompt_tokens = u32::try_from(prompt_chars.div_ceil(3)).unwrap_or(u32::MAX).saturating_add(REPLY_ROOM_OVERHEAD_TOKENS);
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
    role_id: &str,
    prompt: &str,
    session_id: &SessionId,
    image: Option<&str>,
    config_path: Option<&str>,
    profile_name: &str,
    max_completion_tokens: Option<u32>,
) -> Result<(Dispatched, bool)> {
    use darkmux_crew::dispatch::{dispatch, DispatchOpts};
    let opts = DispatchOpts {
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
        // (`replyRoom`) Read only by the one-call path a tool-less role on
        // an unmanaged endpoint takes; `None` keeps its own default.
        max_completion_tokens,
        image: image.map(str::to_string),
        model_base_url_override: None,
        step_id: None, // (#1483) set on the graph-step path only
        system_prompt_override: None,
    };
    let result = dispatch(opts).context("internal-runtime dispatch via lab harness")?;
    // The container path mounts a bookkeeping dir; the one-call path has none.
    let one_call = result.out_dir.is_none();
    Ok((Dispatched { exit_code: result.exit_code, stdout: result.stdout, stderr: result.stderr }, one_call))
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
    let v = loaded.manifest.workload.verify.as_ref()?;
    let Some(fields) = expect_report(loaded, text) else { return Some(keyword_verify(v, text)) };
    // (`verify.expect`) The structured-output check, and the keyword checks
    // too when the workload also lists keywords: both must pass.
    let keywords = (!v.must_contain.is_empty() || !v.must_not_contain.is_empty()).then(|| keyword_verify(v, text));
    let passed = fields.passed() && keywords.as_ref().is_none_or(|k| k.passed);
    let details = [Some(fields.details()), keywords.map(|k| k.details)].into_iter().flatten().collect::<Vec<_>>().join("; ");
    Some(VerifyOutcome { passed, details })
}

/// The workload's `verify.expect` check of `text`, `None` when it has none.
pub(crate) fn expect_report(loaded: &LoadedWorkload, text: &str) -> Option<expect::FieldsReport> {
    let want = loaded.manifest.workload.verify.as_ref()?.expect.as_ref()?;
    Some(expect::grade(want, text))
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
            let r = finish_run(&FinishInputs { loaded: &loaded, run_dir: &run_dir, profile: &profile, profile_name: "p", session_id: &session, duration_ms: 1, reply_cap: None }, &d).unwrap();
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
        let inputs = FinishInputs { loaded: &loaded, run_dir: &run_dir, profile: &profile, profile_name: "p", session_id: &session, duration_ms: 1, reply_cap: Some(30_000) };
        let r = finish_run(&inputs, &d).unwrap();
        assert!(!r.verify.unwrap().passed);
        let m = manifest_of(&run_dir);
        assert_eq!(m["verify"]["passed"], false);
        assert_eq!(m["verify"]["fields_correct"], 2);
        assert_eq!(m["verify"]["fields_expected"], 3);
        assert_eq!(m["verify"]["fields_missed"], serde_json::json!(["b"]));
        assert_eq!(m["max_completion_tokens"], 30_000);

        let plain = make_loaded(spec_with_prompt("x"), tmp.path().to_path_buf());
        let run_dir = tmp.path().join("plain");
        fs::create_dir_all(&run_dir).unwrap();
        let d = Dispatched { exit_code: 0, stdout: envelope("hi"), stderr: String::new() };
        let inputs = FinishInputs { loaded: &plain, run_dir: &run_dir, profile: &profile, profile_name: "p", session_id: &session, duration_ms: 1, reply_cap: None };
        finish_run(&inputs, &d).unwrap();
        let m = manifest_of(&run_dir);
        assert!(m.get("max_completion_tokens").is_none());
        assert!(m["verify"].is_null(), "no verify spec: not checked, as before");
    }

    /// `replyRoom`: the window minus the over-counted prompt and the fixed
    /// overhead; a window the prompt nearly fills is refused, not dispatched.
    #[test]
    fn reply_room_is_the_window_minus_the_over_counted_prompt() {
        assert_eq!(reply_room_from(32_768, 3_000, "w").unwrap(), 32_768 - 1_000 - REPLY_ROOM_OVERHEAD_TOKENS);
        assert_eq!(reply_room_from(8_192, 1, "w").unwrap(), 8_192 - 1 - REPLY_ROOM_OVERHEAD_TOKENS);
        let err = reply_room_from(4_096, 11_000, "w").unwrap_err().to_string();
        assert!(err.contains("raise the profile's n_ctx"), "{err}");
    }

    #[test]
    fn reply_room_needs_the_profile_models_n_ctx_and_is_absent_unless_asked() {
        let tmp = TempDir::new().unwrap();
        let mut profile = darkmux_types::Profile::default();
        profile.models.push(darkmux_types::ProfileModel { id: "m".into(), ..Default::default() });
        let plain = make_loaded(spec_with_prompt("x"), tmp.path().to_path_buf());
        assert_eq!(reply_room_tokens(&plain, &profile, "no-such-role", "x").unwrap(), None);

        let mut spec = spec_with_prompt("x");
        spec.reply_room = Some(ReplyRoom::Context);
        let asks = make_loaded(spec, tmp.path().to_path_buf());
        let err = reply_room_tokens(&asks, &profile, "no-such-role", "x").unwrap_err().to_string();
        assert!(err.contains("declares no n_ctx"), "{err}");
        profile.models[0].n_ctx = Some(16_384);
        assert_eq!(reply_room_tokens(&asks, &profile, "no-such-role", "xyz").unwrap(), Some(16_384 - 1 - REPLY_ROOM_OVERHEAD_TOKENS));
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
}
