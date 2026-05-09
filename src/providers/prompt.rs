//! Trivial workload provider: "just answer this prompt."
//! No sandbox; optional must_contain / must_not_contain keyword checks.
//!
//! Dispatches via the active runtime's CLI (default: `openclaw agent`).

use crate::types::Profile;
use crate::workloads::types::{
    InspectionReport, LoadedWorkload, RunResult, VerifyOutcome, WorkloadProvider,
};
use anyhow::{Context, Result, anyhow, bail};
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct PromptProvider;

impl WorkloadProvider for PromptProvider {
    fn id(&self) -> &'static str {
        "prompt"
    }
    fn description(&self) -> &'static str {
        "Trivial provider: dispatch a single prompt, capture reply, optionally check keywords."
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
        _sandbox_dir: &Path,
        profile: &Profile,
        profile_name: &str,
    ) -> Result<RunResult> {
        let prompt = resolve_prompt(loaded)?;
        let agent = pick_agent(loaded);
        let session_id = format!(
            "darkmux-prompt-{}-{}",
            loaded.manifest.workload.id,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );

        let runtime_cmd = runtime_cmd();
        let started = std::time::Instant::now();
        let output = Command::new(&runtime_cmd)
            .args([
                "agent",
                "--agent",
                &agent,
                "--session-id",
                &session_id,
                "--json",
                "--timeout",
                "3600",
                "--message",
                &prompt,
            ])
            .output()
            .with_context(|| format!("running `{runtime_cmd} agent ...`"))?;
        let duration_ms = started.elapsed().as_millis();
        let ok = output.status.success();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        fs::write(run_dir.join("qa-reply.json"), &stdout)?;
        if !stderr.is_empty() {
            fs::write(run_dir.join("qa-reply.err"), &stderr)?;
        }

        let reply = extract_reply_text(&stdout);
        let verify = run_verify(loaded, &reply);

        let run_id = run_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let manifest_json = serde_json::json!({
            // v2 added: runId, profile (now the profile NAME), profileDescription.
            // v1 had: sessionId, profile (was the description text), workload, provider, durationMs, ok.
            "schemaVersion": 2,
            "runId": run_id,
            "workload": loaded.manifest.workload.id,
            "provider": self.id(),
            "profile": profile_name,
            "profileDescription": profile.description.clone().unwrap_or_default(),
            "durationMs": duration_ms,
            "ok": ok,
            "sessionId": session_id,
        });
        fs::write(
            run_dir.join("manifest.json"),
            serde_json::to_string_pretty(&manifest_json)?,
        )?;

        Ok(RunResult {
            ok,
            duration_ms,
            payload_text: Some(reply),
            trajectory_path: None,
            verify: Some(verify),
            error: if ok { None } else { Some(format!("runtime exit: {stderr}")) },
        })
    }

    fn inspect(&self, loaded: &LoadedWorkload, run_dir: &Path) -> Result<InspectionReport> {
        let manifest_path = run_dir.join("manifest.json");
        let meta = if manifest_path.exists() {
            serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&manifest_path)?)?
        } else {
            serde_json::Value::Null
        };
        let reply_path = run_dir.join("qa-reply.json");
        let reply = if reply_path.exists() {
            extract_reply_text(&fs::read_to_string(&reply_path)?)
        } else {
            String::new()
        };
        let verify_outcome = run_verify(loaded, &reply);
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
            walltime_ms: meta.get("durationMs").and_then(|v| v.as_u64()).unwrap_or(0) as u128,
            turns: 1,
            compactions: 0,
            tokens_before: vec![],
            summary_chars: vec![],
            mode: None,
            notes: vec![
                format!("provider={}", self.id()),
                format!(
                    "verify: {} — {}",
                    if verify_outcome.passed { "ok" } else { "fail" },
                    verify_outcome.details
                ),
            ],
        })
    }
}

fn runtime_cmd() -> String {
    env::var("DARKMUX_RUNTIME_CMD").unwrap_or_else(|_| "openclaw".to_string())
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
        "workload \"{}\" must define prompt or promptFile",
        loaded.manifest.workload.id
    ))
}

fn pick_agent(loaded: &LoadedWorkload) -> String {
    if let Some(a) = loaded.manifest.workload.agent.as_deref() {
        return a.to_string();
    }
    env::var("DARKMUX_DEFAULT_AGENT").unwrap_or_else(|_| "main".to_string())
}

pub(crate) fn extract_reply_text(stdout: &str) -> String {
    if stdout.trim().is_empty() {
        return String::new();
    }
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return stdout.to_string();
    };
    if let Some(payloads) = parsed.get("result").and_then(|r| r.get("payloads")).and_then(|p| p.as_array()) {
        let parts: Vec<String> = payloads
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()).map(|s| s.to_string()))
            .collect();
        return parts.join("\n\n");
    }
    if let Some(reply) = parsed.get("reply").and_then(|v| v.as_str()) {
        return reply.to_string();
    }
    String::new()
}

pub(crate) fn run_verify(loaded: &LoadedWorkload, text: &str) -> VerifyOutcome {
    let v = match loaded.manifest.workload.verify.as_ref() {
        Some(v) => v,
        None => {
            return VerifyOutcome {
                passed: true,
                details: "no verify spec".into(),
            };
        }
    };
    let missing: Vec<&String> = v.must_contain.iter().filter(|s| !text.contains(s.as_str())).collect();
    let present: Vec<&String> = v
        .must_not_contain
        .iter()
        .filter(|s| text.contains(s.as_str()))
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

// Suppress unused warnings until the lab subcommand calls run/inspect from main.
#[allow(dead_code)]
fn __compile_check(_: &dyn WorkloadProvider) {}

#[allow(dead_code)]
fn _bail_unused() -> Result<()> {
    bail!("unused")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workloads::types::{
        VerifySpec, WorkloadManifest, WorkloadSource, WorkloadSpec,
    };
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_loaded(spec: WorkloadSpec, base_dir: PathBuf) -> LoadedWorkload {
        LoadedWorkload {
            manifest: WorkloadManifest { workload: spec },
            manifest_path: base_dir.join("workload.json"),
            base_dir,
            source: WorkloadSource::Builtin,
        }
    }

    fn spec_with_prompt(prompt: &str) -> WorkloadSpec {
        WorkloadSpec {
            id: "t".into(),
            provider: "prompt".into(),
            description: None,
            agent: None,
            prompt: Some(prompt.into()),
            prompt_file: None,
            sandbox_seed: None,
            verify: None,
            expected: None,
            extras: BTreeMap::new(),
        }
    }

    #[test]
    fn provider_metadata() {
        let p = PromptProvider;
        assert_eq!(p.id(), "prompt");
        assert!(p.description().contains("prompt"));
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
    fn pick_agent_from_manifest() {
        let tmp = TempDir::new().unwrap();
        let mut spec = spec_with_prompt("x");
        spec.agent = Some("legal".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        assert_eq!(pick_agent(&loaded), "legal");
    }

    #[test]
    fn pick_agent_default_main() {
        let tmp = TempDir::new().unwrap();
        let loaded = make_loaded(spec_with_prompt("x"), tmp.path().to_path_buf());
        unsafe { env::remove_var("DARKMUX_DEFAULT_AGENT") };
        assert_eq!(pick_agent(&loaded), "main");
    }

    #[test]
    fn extract_reply_handles_payloads_array() {
        let json = r#"{"result":{"payloads":[{"text":"hello"},{"text":"world"}]}}"#;
        assert_eq!(extract_reply_text(json), "hello\n\nworld");
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
    fn run_verify_passes_with_no_spec() {
        let tmp = TempDir::new().unwrap();
        let loaded = make_loaded(spec_with_prompt("x"), tmp.path().to_path_buf());
        let v = run_verify(&loaded, "anything");
        assert!(v.passed);
        assert!(v.details.contains("no verify"));
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
        let v = run_verify(&loaded, "we have alpha and beta here");
        assert!(v.passed);
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
        let v = run_verify(&loaded, "alpha here only");
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
        let v = run_verify(&loaded, "this contains forbidden text");
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
        let v = run_verify(&loaded, "this has bad words but no required marker");
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
        PromptProvider.setup(&loaded, &run_dir, &sandbox_dir).unwrap();
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

    /// Forward-compat: when the manifest carries a `runId`, inspect returns
    /// that value rather than the dir basename. Locks the new schema (v2).
    #[test]
    fn inspect_uses_run_id_from_manifest() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("dir-name-differs");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("manifest.json"),
            r#"{"schemaVersion":2,"runId":"the-canonical-id","workload":"t","provider":"prompt","profile":"deep","durationMs":42,"ok":true,"sessionId":"darkmux-prompt-t-1"}"#,
        )
        .unwrap();
        let loaded = make_loaded(spec_with_prompt("x"), tmp.path().to_path_buf());
        let report = PromptProvider.inspect(&loaded, &run_dir).unwrap();
        assert_eq!(report.run_id, "the-canonical-id");
        assert_eq!(report.workload_id, "t");
        assert_eq!(report.walltime_ms, 42);
    }
}
