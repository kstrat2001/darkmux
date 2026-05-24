//! Coding-task workload provider — ports today's lab harness (run-open.sh,
//! run-3.sh, run-probe-recall.sh) to Rust.
//!
//! Setup: copy a sandbox seed into `.darkmux/sandboxes/<workload>/`.
//! Run: dispatch via the active runtime, capture trajectory + reply.
//! Inspect: parse trajectory, identify compactions, classify mode.

use crate::providers::prompt::{extract_reply_text, run_verify};
use crate::types::Profile;
use crate::workloads::types::{
    InspectionReport, LoadedWorkload, RunMode, RunResult, WorkloadProvider,
};
use anyhow::{anyhow, bail, Context, Result};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct CodingTaskProvider;

impl WorkloadProvider for CodingTaskProvider {
    fn id(&self) -> &'static str {
        "coding-task"
    }
    fn description(&self) -> &'static str {
        "Coding workload: prompt + sandbox seed + verification command (e.g. npm test)."
    }

    fn setup(&self, loaded: &LoadedWorkload, run_dir: &Path, sandbox_dir: &Path) -> Result<()> {
        if !run_dir.exists() {
            fs::create_dir_all(run_dir)?;
        }
        if !sandbox_dir.exists() {
            fs::create_dir_all(sandbox_dir)?;
        }

        // 1. Apply external sandbox seed (sibling directory referenced
        //    via `sandboxSeed`). Embedded workloads can't use this
        //    because include_str! only handles individual files.
        if let Some(seed_rel) = manifest_seed_path(loaded) {
            let seed_path = loaded.base_dir.join(&seed_rel);
            if seed_path.exists() {
                copy_dir_recursive(&seed_path, sandbox_dir)
                    .with_context(|| format!("seeding sandbox from {}", seed_path.display()))?;
            }
        }

        // 2. Apply inline setupContent (works with embedded workloads).
        //    Writes each (relative-path → content) pair into the
        //    sandbox dir, creating parent directories as needed.
        //
        //    Precedence: setupContent OVERLAYS on top of sandboxSeed.
        //    If both target the same file, setupContent wins. This lets
        //    an embedded workload patch a specific file over a copied
        //    seed directory.
        //
        //    Re-applies on every dispatch (no skip-if-exists). The
        //    sandbox is operator-mutated by each run (agent edits land
        //    on disk); re-applying setupContent gives every dispatch a
        //    deterministic starting point so re-runs don't measure
        //    cached agent edits as "instant fixes."
        if !loaded.manifest.workload.setup_content.is_empty() {
            for (rel_path, content) in &loaded.manifest.workload.setup_content {
                // Path-traversal hardening: reject absolute paths, `..`
                // components, and any key that would resolve outside
                // sandbox_dir. Embedded workloads are trusted (compiled
                // in), but `~/.darkmux/workloads/<id>.json` is operator-
                // installed and may come from a gist / friend / future
                // install verb. Validate at the receiver, not at install.
                validate_setup_content_key(rel_path).with_context(|| {
                    format!("setupContent key `{rel_path}` is unsafe")
                })?;
                let target = sandbox_dir.join(rel_path);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!(
                            "creating parent dir {} for setupContent file {}",
                            parent.display(),
                            rel_path
                        )
                    })?;
                }
                fs::write(&target, content).with_context(|| {
                    format!("writing setupContent file {}", target.display())
                })?;
            }
            // Operator-visible signal that the sandbox was reset before
            // the agent ran. Without this, an operator looking at a
            // mutated `~/.darkmux/sandboxes/<workload>/` couldn't tell
            // whether the dispatch ran against fresh state or stale
            // post-prior-edit state.
            eprintln!(
                "[lab] setupContent re-applied to {} ({} file(s))",
                sandbox_dir.display(),
                loaded.manifest.workload.setup_content.len()
            );
        }

        // 3. Loud-fail when the workload declares requiresExternalSandbox
        //    but the sandbox is empty AND no inline setup content was
        //    applied. Without this check, the dispatch would proceed
        //    against an empty workspace and the agent would hallucinate
        //    files that don't exist — wasting wall-clock for unactionable
        //    output. Operator-actionable hint names the env var.
        if loaded.manifest.workload.requires_external_sandbox
            && loaded.manifest.workload.setup_content.is_empty()
            && sandbox_is_empty(sandbox_dir)
        {
            let env_key = format!(
                "DARKMUX_SANDBOX_{}",
                loaded
                    .manifest
                    .workload
                    .id
                    .replace('-', "_")
                    .to_ascii_uppercase()
            );
            bail!(
                "workload `{}` requires an external sandbox but `{}` is empty.\n\
                 \n\
                 This workload expects a pre-existing project (e.g. a Node repo with the source\n\
                 files the prompt references). Two ways forward:\n\
                 \n\
                   1. Point at an existing project on disk:\n\
                        export {}=<path-to-your-project>\n\
                 \n\
                   2. If you don't have a fitting project, see the workload manifest's `_comment`\n\
                      field for the expected sandbox shape:\n\
                        darkmux lab workloads | grep `{}`\n\
                 \n\
                 For a coding-task workload that runs out of the box (no external setup), try:\n\
                   darkmux lab run quick-coding",
                loaded.manifest.workload.id,
                sandbox_dir.display(),
                env_key,
                loaded.manifest.workload.id,
            );
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
        runtime: crate::crew::dispatch::Runtime,
        runtime_cmd: &str,
    ) -> Result<RunResult> {
        // Per-runtime sandbox-path substitution:
        //   - Openclaw runs on host → agent sees the host sandbox path.
        //   - Internal runtime mounts sandbox_dir at /workspace in the
        //     container → agent sees /workspace. Substituting the host
        //     path here would point the agent at a path invisible
        //     inside Docker (#337 root cause).
        let raw_prompt = resolve_prompt(loaded)?;
        let prompt = match runtime {
            crate::crew::dispatch::Runtime::Internal => {
                expand_placeholders_with(&raw_prompt, "/workspace")
            }
            crate::crew::dispatch::Runtime::Openclaw => {
                expand_placeholders(&raw_prompt, sandbox_dir)
            }
        };
        let role = pick_role(loaded);
        let session_id = format!(
            "darkmux-coding-{}-{}",
            loaded.manifest.workload.id,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );

        let started = std::time::Instant::now();
        let (stdout, stderr, ok) = match runtime {
            crate::crew::dispatch::Runtime::Internal => {
                // Pass sandbox_dir as --workdir so the runtime mounts
                // it at /workspace, matching the placeholder
                // substitution above (#337 fix).
                dispatch_via_internal(&role, &prompt, &session_id, Some(sandbox_dir.to_path_buf()))?
            }
            crate::crew::dispatch::Runtime::Openclaw => {
                dispatch_via_openclaw(runtime_cmd, &role, &prompt, &session_id)?
            }
        };
        let duration_ms = started.elapsed().as_millis();

        fs::write(run_dir.join("qa-reply.json"), &stdout)?;
        if !stderr.is_empty() {
            fs::write(run_dir.join("qa-reply.err"), &stderr)?;
        }

        // Best-effort copy of the trajectory before any next dispatch wipes it.
        let mut trajectory_path: Option<PathBuf> = None;
        if let Some(t) = guess_trajectory_path(&session_id) {
            let dst = run_dir.join("trajectory.jsonl");
            if let Err(e) = fs::copy(&t, &dst) {
                eprintln!("darkmux: warn — failed copying trajectory: {e}");
            } else {
                trajectory_path = Some(dst);
            }
        }

        let verify_outcome = run_verify_command(loaded, run_dir, sandbox_dir)?;

        let run_id = run_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let manifest_json = serde_json::json!({
            // v2 added: runId, profile (now the profile NAME), profileDescription.
            // v1 had: sessionId, profile (was the description text), workload, provider, durationMs, ok, sandbox.
            "schemaVersion": 2,
            "runId": run_id,
            "workload": loaded.manifest.workload.id,
            "provider": self.id(),
            "profile": profile_name,
            "profileDescription": profile.description.clone().unwrap_or_default(),
            "durationMs": duration_ms,
            "ok": ok,
            "sessionId": session_id,
            "sandbox": pathdiff::diff_paths(sandbox_dir, env::current_dir().unwrap_or_else(|_| PathBuf::from("."))).map(|p| p.display().to_string()).unwrap_or_else(|| sandbox_dir.display().to_string()),
        });
        fs::write(
            run_dir.join("manifest.json"),
            serde_json::to_string_pretty(&manifest_json)?,
        )?;

        Ok(RunResult {
            ok,
            duration_ms,
            payload_text: Some(extract_reply_text(&stdout)),
            trajectory_path,
            verify: verify_outcome,
            error: if ok {
                None
            } else {
                Some(format!("runtime exit: {stderr}"))
            },
        })
    }

    fn inspect(&self, loaded: &LoadedWorkload, run_dir: &Path) -> Result<InspectionReport> {
        let manifest_path = run_dir.join("manifest.json");
        let meta = if manifest_path.exists() {
            serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&manifest_path)?)?
        } else {
            serde_json::Value::Null
        };
        let traj_path = run_dir.join("trajectory.jsonl");
        let events = if traj_path.exists() {
            read_jsonl(&traj_path)
        } else {
            Vec::new()
        };

        let prompt_submitted: Vec<&serde_json::Value> = events
            .iter()
            .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some("prompt.submitted"))
            .collect();
        let turns = prompt_submitted.len() as u32;

        let mut tokens_before: Vec<u64> = Vec::new();
        let mut summary_chars: Vec<u64> = Vec::new();
        let mut seen_summaries = std::collections::HashSet::new();
        for ev in &prompt_submitted {
            if let Some(msgs) = ev
                .get("data")
                .and_then(|d| d.get("messages"))
                .and_then(|m| m.as_array())
            {
                for m in msgs {
                    if m.get("role").and_then(|r| r.as_str()) == Some("compactionSummary") {
                        let summary_str = m.get("summary").and_then(|s| s.as_str()).unwrap_or("");
                        if summary_str.is_empty() {
                            continue;
                        }
                        let key: String = summary_str.chars().take(80).collect();
                        if seen_summaries.insert(key) {
                            tokens_before
                                .push(m.get("tokensBefore").and_then(|v| v.as_u64()).unwrap_or(0));
                            summary_chars.push(summary_str.len() as u64);
                        }
                    }
                }
            }
        }

        let walltime_ms = meta.get("durationMs").and_then(|v| v.as_u64()).unwrap_or(0) as u128;
        let mode = classify_mode(walltime_ms, loaded);

        let compactions = tokens_before.len() as u32;
        let mut notes = vec![
            format!("turns={}", turns),
            format!("compactions={}", compactions),
            format!("walltime={}s", walltime_ms / 1000),
        ];
        if let Some(m) = mode {
            notes.push(format!(
                "mode={}",
                match m {
                    RunMode::Fast => "fast",
                    RunMode::Slow => "slow",
                }
            ));
        }
        let verify = run_verify(
            loaded,
            &extract_reply_text(
                &fs::read_to_string(run_dir.join("qa-reply.json")).unwrap_or_default(),
            ),
        );
        notes.push(format!(
            "verify: {}",
            if verify.passed { "ok" } else { "fail" }
        ));

        let run_id = meta
            .get("runId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
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
            walltime_ms,
            turns,
            compactions,
            tokens_before,
            summary_chars,
            mode,
            notes,
        })
    }
}

/// Substitute `${SANDBOX_DIR}` and `${SANDBOX}` in a workload prompt or
/// command with the resolved sandbox path. This is what lets a workload
/// manifest be portable — the manifest references `${SANDBOX_DIR}` and the
/// runtime supplies the actual on-disk path (which may come from the
/// `DARKMUX_SANDBOX_<workload>` env var).
///
/// For prompts dispatched through the internal Docker-bounded runtime
/// (where `sandbox_dir` is mounted at `/workspace` inside the container —
/// see `src/crew/dispatch_internal.rs:99`), callers should use
/// `expand_placeholders_with("/workspace")` instead. Otherwise the agent
/// reads the prompt's host path, can't find files at that path inside
/// the container, and produces an empty trajectory. Verify commands
/// run on the host and continue to use the host path.
fn expand_placeholders(input: &str, sandbox_dir: &Path) -> String {
    expand_placeholders_with(input, &sandbox_dir.display().to_string())
}

/// Lower-level helper: substitute `${SANDBOX_DIR}` / `${SANDBOX}` with
/// an explicit view-path string. Used to swap between host paths
/// (openclaw / verify command) and container-internal paths (internal
/// runtime agent prompts) at the call site.
fn expand_placeholders_with(input: &str, view_path: &str) -> String {
    input
        .replace("${SANDBOX_DIR}", view_path)
        .replace("${SANDBOX}", view_path)
}

fn manifest_seed_path(loaded: &LoadedWorkload) -> Option<String> {
    if let Some(s) = loaded.manifest.workload.sandbox_seed.as_ref() {
        return Some(s.clone());
    }
    Some("sandbox".to_string())
}

/// True when `path` either doesn't exist, isn't a directory, or is an
/// empty directory. Used by the requires-external-sandbox loud-fail
/// check to distinguish "operator set up the sandbox" from "operator
/// expected the workload to magically work."
fn sandbox_is_empty(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut iter) => iter.next().is_none(),
        Err(_) => true,
    }
}

/// Reject setupContent keys that would write outside the sandbox dir.
///
/// Three classes of attack-shape this catches:
///   1. **Absolute paths**: `PathBuf::from(sandbox).join("/etc/passwd")`
///      returns `/etc/passwd` — `Path::join` silently replaces when the
///      arg is absolute. A workload manifest's setupContent key of
///      `"/etc/cron.d/evil"` would write to system state.
///   2. **Parent traversal**: `"../../etc/shadow"` walks out of the
///      sandbox via `create_dir_all` + `write`.
///   3. **Windows drive letters / UNC prefixes**: same risk class as
///      absolute paths on Windows. Reject defensively even though
///      darkmux is Apple-Silicon-tested.
///
/// Embedded workloads (compiled-in JSON via `include_str!`) are trusted
/// by construction. Operator-installed workloads under
/// `~/.darkmux/workloads/<id>.json` may come from anywhere; validate at
/// the consumer rather than at install. The cost is tiny and the
/// invariant is much stronger.
fn validate_setup_content_key(key: &str) -> Result<()> {
    if key.is_empty() {
        bail!("setupContent key is empty");
    }
    let path = Path::new(key);
    if path.is_absolute() {
        bail!(
            "setupContent key `{key}` is an absolute path; only relative paths under the sandbox are allowed"
        );
    }
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::Normal(_) | Component::CurDir => continue,
            Component::ParentDir => bail!(
                "setupContent key `{key}` contains `..` — would escape the sandbox"
            ),
            Component::Prefix(_) | Component::RootDir => bail!(
                "setupContent key `{key}` contains a root/prefix component — only relative paths under the sandbox are allowed"
            ),
        }
    }
    Ok(())
}

fn resolve_prompt(loaded: &LoadedWorkload) -> Result<String> {
    if let Some(p) = loaded.manifest.workload.prompt.as_ref() {
        return Ok(p.clone());
    }
    if let Some(rel) = loaded.manifest.workload.prompt_file.as_ref() {
        let path = loaded.base_dir.join(rel);
        return fs::read_to_string(&path)
            .with_context(|| format!("reading promptFile at {}", path.display()));
    }
    Err(anyhow!(
        "coding-task workload \"{}\" must define prompt or promptFile",
        loaded.manifest.workload.id
    ))
}

/// Resolve which darkmux role to dispatch the coding-task workload
/// through. Beat 36 directional principle: workloads reference DM role
/// manifest ids, not OC agent personas. Default: `code-reviewer` —
/// best-suited to the long-agentic review-shaped tasks coding_task
/// workloads exercise. Override via the workload's `role:` field or
/// `DARKMUX_DEFAULT_ROLE`.
fn pick_role(loaded: &LoadedWorkload) -> String {
    if let Some(r) = loaded.manifest.workload.role.as_deref() {
        return r.to_string();
    }
    env::var("DARKMUX_DEFAULT_ROLE").unwrap_or_else(|_| "code-reviewer".to_string())
}

/// Dispatch via darkmux's internal Docker-bounded runtime through the
/// crew::dispatch substrate. `workdir` (when `Some`) passes as
/// `--workdir` so the runtime mounts the host path at `/workspace`
/// inside the container, giving the agent access to the workload's
/// sandbox files (#337 fix).
fn dispatch_via_internal(
    role_id: &str,
    prompt: &str,
    session_id: &str,
    workdir: Option<PathBuf>,
) -> Result<(String, String, bool)> {
    use crate::crew::dispatch::{dispatch, DispatchOpts, Runtime};
    let opts = DispatchOpts {
        role_id: role_id.to_string(),
        message: prompt.to_string(),
        deliver: None,
        session_id: Some(session_id.to_string()),
        timeout_seconds: 3600,
        skip_preflight: false,
        json: true,
        watch_paths: Vec::new(),
        workdir,
        sprint_id: None,
        runtime: Runtime::Internal,
        runtime_cmd: "openclaw".to_string(),
        machine: None,
        wait: true,
    };
    let result = dispatch(opts).context("internal-runtime dispatch via lab harness")?;
    Ok((result.stdout, result.stderr, result.exit_code == 0))
}

/// Dispatch via the legacy openclaw shell-out path. Shells out with the
/// `<cmd> agent --agent <role> --json ...` calling convention.
/// `runtime_cmd` is the operator-supplied binary path (Sprint-E:
/// `--runtime-cmd <path>` flag; defaults to `"openclaw"`).
fn dispatch_via_openclaw(
    runtime_cmd: &str,
    role: &str,
    prompt: &str,
    session_id: &str,
) -> Result<(String, String, bool)> {
    let output = Command::new(runtime_cmd)
        .args([
            "agent",
            "--agent",
            role,
            "--session-id",
            session_id,
            "--json",
            "--timeout",
            "3600",
            "--message",
            prompt,
        ])
        .output()
        .with_context(|| format!("running `{runtime_cmd} agent ...`"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Ok((stdout, stderr, output.status.success()))
}

/// Best-effort trajectory lookup for the active runtime.
///
/// darkmux doesn't know where every agent runtime stores trajectories, so we
/// look in the location the OpenClaw runtime uses by default. Override with
/// `DARKMUX_RUNTIME_AGENTS_DIR` for any other runtime that stores per-agent
/// session files in a parallel layout: `<dir>/<agent>/sessions/<session-id>.trajectory.jsonl`.
fn guess_trajectory_path(session_id: &str) -> Option<PathBuf> {
    let agents_dir = if let Ok(custom) = env::var("DARKMUX_RUNTIME_AGENTS_DIR") {
        PathBuf::from(custom)
    } else {
        dirs::home_dir()?.join(".openclaw").join("agents")
    };
    if !agents_dir.exists() {
        return None;
    }
    let entries = fs::read_dir(&agents_dir).ok()?;
    for entry in entries.flatten() {
        let candidate = entry
            .path()
            .join("sessions")
            .join(format!("{session_id}.trajectory.jsonl"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn run_verify_command(
    loaded: &LoadedWorkload,
    run_dir: &Path,
    sandbox_dir: &Path,
) -> Result<Option<crate::workloads::types::VerifyOutcome>> {
    let Some(spec) = loaded.manifest.workload.verify.as_ref() else {
        return Ok(None);
    };
    let Some(cmd_raw) = spec.command.as_ref() else {
        return Ok(None);
    };
    // Same ${SANDBOX_DIR} substitution as the prompt — verify commands
    // commonly need to cd into the sandbox or reference files there.
    let cmd = expand_placeholders(cmd_raw, sandbox_dir);
    let cwd = if let Some(rel) = spec.cwd.as_ref() {
        sandbox_dir.join(expand_placeholders(rel, sandbox_dir))
    } else {
        sandbox_dir.to_path_buf()
    };
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(&cmd)
        .current_dir(&cwd)
        .output()
        .with_context(|| format!("running verify command: {cmd}"))?;
    let merged = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(run_dir.join("verify-output.txt"), &merged)?;
    Ok(Some(crate::workloads::types::VerifyOutcome {
        passed: output.status.success(),
        details: if output.status.success() {
            "verify command exited 0".into()
        } else {
            format!("exit {}", output.status.code().unwrap_or(-1))
        },
    }))
}

fn classify_mode(walltime_ms: u128, loaded: &LoadedWorkload) -> Option<RunMode> {
    let ex = loaded.manifest.workload.expected.as_ref()?;
    let seconds = (walltime_ms / 1000) as u64;
    if let Some((_, hi)) = ex.fast_cluster_seconds {
        if seconds <= hi {
            return Some(RunMode::Fast);
        }
    }
    if let Some((lo, _)) = ex.slow_cluster_seconds {
        if seconds >= lo {
            return Some(RunMode::Slow);
        }
    }
    None
}

fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    if !dst.exists() {
        fs::create_dir_all(dst)?;
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if s.is_dir() {
            copy_dir_recursive(&s, &d)?;
        } else {
            fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn _unused_bail() -> Result<()> {
    bail!("unused")
}

// Tiny dependency-free stand-in for `pathdiff` (don't add a crate just for one fn).
mod pathdiff {
    use std::path::{Path, PathBuf};
    pub fn diff_paths<P: AsRef<Path>, B: AsRef<Path>>(path: P, base: B) -> Option<PathBuf> {
        let path = path.as_ref();
        let base = base.as_ref();
        if path == base {
            return Some(PathBuf::from("."));
        }
        if let Ok(stripped) = path.strip_prefix(base) {
            return Some(stripped.to_path_buf());
        }
        Some(path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workloads::types::{
        ExpectedSpec, VerifySpec, WorkloadManifest, WorkloadSource, WorkloadSpec,
    };
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn make_loaded(spec: WorkloadSpec, base_dir: PathBuf) -> LoadedWorkload {
        LoadedWorkload {
            manifest: WorkloadManifest { workload: spec },
            manifest_path: base_dir.join("workload.json"),
            base_dir,
            source: WorkloadSource::Builtin,
        }
    }

    fn basic_spec() -> WorkloadSpec {
        WorkloadSpec {
            id: "coding".into(),
            provider: "coding-task".into(),
            description: None,
            role: None,
            prompt: Some("write tests".into()),
            prompt_file: None,
            sandbox_seed: None,
            setup_content: BTreeMap::new(),
            requires_external_sandbox: false,
            verify: None,
            expected: None,
            extras: BTreeMap::new(),
        }
    }

    #[test]
    fn provider_metadata() {
        let p = CodingTaskProvider;
        assert_eq!(p.id(), "coding-task");
        assert!(p.description().contains("sandbox"));
    }

    #[test]
    fn manifest_seed_path_default_is_sandbox() {
        let tmp = TempDir::new().unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        assert_eq!(manifest_seed_path(&loaded), Some("sandbox".to_string()));
    }

    #[test]
    fn manifest_seed_path_uses_explicit() {
        let tmp = TempDir::new().unwrap();
        let mut spec = basic_spec();
        spec.sandbox_seed = Some("custom-seed".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        assert_eq!(manifest_seed_path(&loaded), Some("custom-seed".into()));
    }

    #[test]
    fn pick_role_default_code_reviewer() {
        let tmp = TempDir::new().unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        unsafe { env::remove_var("DARKMUX_DEFAULT_ROLE") };
        assert_eq!(pick_role(&loaded), "code-reviewer");
    }

    #[test]
    fn pick_role_from_manifest() {
        let tmp = TempDir::new().unwrap();
        let mut spec = basic_spec();
        spec.role = Some("analyst".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        assert_eq!(pick_role(&loaded), "analyst");
    }

    #[test]
    fn copy_dir_recursive_works() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("a.txt"), "alpha").unwrap();
        fs::write(src.join("nested/b.txt"), "bravo").unwrap();
        copy_dir_recursive(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "alpha");
        assert_eq!(
            fs::read_to_string(dst.join("nested/b.txt")).unwrap(),
            "bravo"
        );
    }

    #[test]
    fn copy_dir_recursive_creates_dst() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("not-yet-there/dst");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("x.txt"), "x").unwrap();
        copy_dir_recursive(&src, &dst).unwrap();
        assert!(dst.exists());
        assert_eq!(fs::read_to_string(dst.join("x.txt")).unwrap(), "x");
    }

    #[test]
    fn read_jsonl_skips_blanks_and_malformed() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("trace.jsonl");
        fs::write(
            &p,
            r#"{"a":1}
not-valid-json
{"b":2}

{"c":3}
"#,
        )
        .unwrap();
        let parsed = read_jsonl(&p);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0]["a"], 1);
        assert_eq!(parsed[2]["c"], 3);
    }

    #[test]
    fn read_jsonl_returns_empty_on_missing() {
        let parsed = read_jsonl(Path::new("/nonexistent/path.jsonl"));
        assert!(parsed.is_empty());
    }

    #[test]
    fn classify_mode_fast() {
        let tmp = TempDir::new().unwrap();
        let mut spec = basic_spec();
        spec.expected = Some(ExpectedSpec {
            fast_cluster_seconds: Some((197, 280)),
            slow_cluster_seconds: Some((600, 950)),
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        assert_eq!(classify_mode(220_000, &loaded), Some(RunMode::Fast));
    }

    #[test]
    fn classify_mode_slow() {
        let tmp = TempDir::new().unwrap();
        let mut spec = basic_spec();
        spec.expected = Some(ExpectedSpec {
            fast_cluster_seconds: Some((197, 280)),
            slow_cluster_seconds: Some((600, 950)),
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        assert_eq!(classify_mode(700_000, &loaded), Some(RunMode::Slow));
    }

    #[test]
    fn classify_mode_unclassified() {
        let tmp = TempDir::new().unwrap();
        let mut spec = basic_spec();
        spec.expected = Some(ExpectedSpec {
            fast_cluster_seconds: Some((197, 280)),
            slow_cluster_seconds: Some((600, 950)),
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        // 400s is between fast (max 280) and slow (min 600)
        assert_eq!(classify_mode(400_000, &loaded), None);
    }

    #[test]
    fn classify_mode_no_expected_returns_none() {
        let tmp = TempDir::new().unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        assert_eq!(classify_mode(1_000_000, &loaded), None);
    }

    #[test]
    fn setup_creates_dirs_when_no_seed() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .unwrap();
        assert!(run_dir.exists());
        assert!(sandbox_dir.exists());
    }

    #[test]
    fn setup_copies_seed_when_present() {
        let tmp = TempDir::new().unwrap();
        // Create seed at base_dir/sandbox/foo.txt
        let seed_dir = tmp.path().join("sandbox");
        fs::create_dir_all(&seed_dir).unwrap();
        fs::write(seed_dir.join("foo.txt"), "seeded").unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox-out");
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .unwrap();
        assert!(sandbox_dir.join("foo.txt").exists());
        assert_eq!(
            fs::read_to_string(sandbox_dir.join("foo.txt")).unwrap(),
            "seeded"
        );
    }

    /// `setupContent` writes each (path → content) pair into the sandbox
    /// dir at setup() time. Lets embedded workloads ship a complete
    /// runnable scaffold without needing an external project.
    #[test]
    fn setup_content_writes_inline_files_to_sandbox() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let mut spec = basic_spec();
        spec.setup_content
            .insert("bug.py".into(), "def buggy():\n    return None\n".into());
        spec.setup_content.insert(
            "tests/test_bug.py".into(),
            "import unittest\n\n# nested path created\n".into(),
        );
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .unwrap();
        assert_eq!(
            fs::read_to_string(sandbox_dir.join("bug.py")).unwrap(),
            "def buggy():\n    return None\n"
        );
        assert!(sandbox_dir.join("tests/test_bug.py").exists());
    }

    /// `requiresExternalSandbox` + empty sandbox + no inline setupContent
    /// → bail with operator-actionable error. This catches the new-user
    /// failure mode where the workload prompt references files the
    /// operator hasn't provided.
    #[test]
    fn setup_bails_loud_when_external_sandbox_required_but_empty() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let mut spec = basic_spec();
        spec.id = "long-agentic-style".into();
        spec.requires_external_sandbox = true;
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let err = CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("requires an external sandbox"),
            "expected actionable error; got: {msg}"
        );
        assert!(
            msg.contains("DARKMUX_SANDBOX_LONG_AGENTIC_STYLE"),
            "expected env-var hint with derived name; got: {msg}"
        );
        assert!(
            msg.contains("quick-coding"),
            "expected fallback pointer to quick-coding; got: {msg}"
        );
    }

    /// `requiresExternalSandbox` is a no-op when inline setupContent
    /// satisfies the dependency — embedded workloads can declare the
    /// flag for documentation purposes without breaking the run.
    #[test]
    fn setup_does_not_bail_when_setup_content_satisfies_external_requirement() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let mut spec = basic_spec();
        spec.requires_external_sandbox = true;
        spec.setup_content
            .insert("file.txt".into(), "content".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .expect("setupContent should satisfy requires_external_sandbox");
    }

    /// Path-traversal hardening on setupContent keys (QA BLOCK fix).
    /// An operator-installed workload manifest from a gist / friend /
    /// future install verb might include an absolute or `..`-walking
    /// key. The provider must reject before writing.
    #[test]
    fn setup_content_rejects_absolute_path() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let mut spec = basic_spec();
        spec.setup_content
            .insert("/etc/passwd".into(), "evil".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let err = CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("setupContent key") && msg.contains("/etc/passwd"),
            "expected error to name the offending key; got: {msg}"
        );
        assert!(
            !std::path::Path::new("/etc/passwd-evil-test").exists(),
            "sanity: no host file should have been written"
        );
    }

    #[test]
    fn setup_content_rejects_parent_traversal() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let mut spec = basic_spec();
        spec.setup_content
            .insert("../escape.txt".into(), "evil".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let err = CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .unwrap_err();
        assert!(
            err.to_string().contains(".."),
            "expected error to mention the traversal component; got: {err}"
        );
        assert!(
            !sandbox_dir.parent().unwrap().join("escape.txt").exists(),
            "no file should have been written outside sandbox"
        );
    }

    #[test]
    fn setup_content_accepts_safe_nested_path() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let mut spec = basic_spec();
        spec.setup_content
            .insert("a/b/c.txt".into(), "ok".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .expect("nested relative paths must be allowed");
        assert_eq!(
            fs::read_to_string(sandbox_dir.join("a/b/c.txt")).unwrap(),
            "ok"
        );
    }

    /// QA reviewer's recommendation: pin the per-runtime substitution
    /// contract. `${SANDBOX_DIR}` resolves to the operator-supplied
    /// view_path; verify substitution + the dispatch-side branching
    /// produce different paths for openclaw (host) vs internal
    /// (`/workspace`).
    #[test]
    fn expand_placeholders_with_substitutes_view_path() {
        let host = "/Users/kain/.darkmux/sandboxes/quick-coding";
        let inside_container = "/workspace";
        let prompt = "Fix the bug in ${SANDBOX_DIR}/bug.py — run python3 ${SANDBOX}/test.py";

        let host_view = expand_placeholders_with(prompt, host);
        assert!(host_view.contains(host), "openclaw path should be substituted");
        assert!(!host_view.contains("/workspace"));

        let container_view = expand_placeholders_with(prompt, inside_container);
        assert!(
            container_view.contains("/workspace/bug.py")
                && container_view.contains("/workspace/test.py")
        );
        assert!(
            !container_view.contains("/Users/"),
            "container view must NOT leak host path"
        );
    }

    #[test]
    fn inspect_handles_no_trajectory() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&run_dir).unwrap();
        // Provide minimal manifest.json
        fs::write(
            run_dir.join("manifest.json"),
            r#"{"sessionId":"abc","durationMs":12345}"#,
        )
        .unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        let report = CodingTaskProvider.inspect(&loaded, &run_dir).unwrap();
        // Manifest pre-dates `runId`, so inspect falls back to the dir basename.
        assert_eq!(report.run_id, "run");
        assert_eq!(report.walltime_ms, 12345);
        assert_eq!(report.turns, 0);
        assert_eq!(report.compactions, 0);
    }

    /// Forward-compat: a v2-shaped manifest with `runId` should be returned
    /// as-is, not the dir basename.
    #[test]
    fn inspect_uses_run_id_from_manifest() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("dir-name-differs");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("manifest.json"),
            r#"{"schemaVersion":2,"runId":"the-canonical-id","sessionId":"sess-1","durationMs":7000}"#,
        )
        .unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        let report = CodingTaskProvider.inspect(&loaded, &run_dir).unwrap();
        assert_eq!(report.run_id, "the-canonical-id");
        assert_eq!(report.walltime_ms, 7000);
    }

    #[test]
    fn inspect_counts_turns_and_compactions() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("manifest.json"),
            r#"{"sessionId":"sess","durationMs":300000}"#,
        )
        .unwrap();
        // Three prompt.submitted events; two of them carry a unique compactionSummary.
        let trajectory = r#"{"type":"prompt.submitted","data":{"messages":[]}}
{"type":"prompt.submitted","data":{"messages":[{"role":"compactionSummary","summary":"alpha summary","tokensBefore":48000}]}}
{"type":"prompt.submitted","data":{"messages":[{"role":"compactionSummary","summary":"alpha summary","tokensBefore":48000}]}}
{"type":"prompt.submitted","data":{"messages":[{"role":"compactionSummary","summary":"beta summary text","tokensBefore":50000}]}}
"#;
        fs::write(run_dir.join("trajectory.jsonl"), trajectory).unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        let report = CodingTaskProvider.inspect(&loaded, &run_dir).unwrap();
        assert_eq!(report.turns, 4);
        assert_eq!(report.compactions, 2); // dedup by 80-char prefix
        assert_eq!(report.tokens_before, vec![48000, 50000]);
    }

    #[test]
    fn inspect_classifies_fast_when_expected_set() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("manifest.json"),
            r#"{"sessionId":"x","durationMs":220000}"#,
        )
        .unwrap();
        let mut spec = basic_spec();
        spec.expected = Some(ExpectedSpec {
            fast_cluster_seconds: Some((197, 280)),
            slow_cluster_seconds: Some((600, 950)),
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let report = CodingTaskProvider.inspect(&loaded, &run_dir).unwrap();
        assert_eq!(report.mode, Some(RunMode::Fast));
    }

    #[test]
    fn pathdiff_relative() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let inside = base.join("sub/dir");
        let rel = pathdiff::diff_paths(&inside, base).unwrap();
        assert_eq!(rel, PathBuf::from("sub/dir"));
    }

    #[test]
    fn pathdiff_same() {
        let tmp = TempDir::new().unwrap();
        let rel = pathdiff::diff_paths(tmp.path(), tmp.path()).unwrap();
        assert_eq!(rel, PathBuf::from("."));
    }

    #[test]
    fn run_verify_command_returns_none_when_no_command() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&run_dir).unwrap();
        let mut spec = basic_spec();
        spec.verify = Some(VerifySpec::default());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let result = run_verify_command(&loaded, &run_dir, tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn run_verify_command_executes_command() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&run_dir).unwrap();
        let mut spec = basic_spec();
        spec.verify = Some(VerifySpec {
            command: Some("true".into()),
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let result = run_verify_command(&loaded, &run_dir, tmp.path())
            .unwrap()
            .unwrap();
        assert!(result.passed);
    }

    #[test]
    fn run_verify_command_reports_failure() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&run_dir).unwrap();
        let mut spec = basic_spec();
        spec.verify = Some(VerifySpec {
            command: Some("false".into()),
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let result = run_verify_command(&loaded, &run_dir, tmp.path())
            .unwrap()
            .unwrap();
        assert!(!result.passed);
        assert!(result.details.contains("exit"));
    }
}
