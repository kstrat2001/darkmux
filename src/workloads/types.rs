//! WorkloadProvider trait + manifest schema.
//!
//! A workload is **any prompt + optional setup + measurement criteria**.
//! Not just coding tasks. The provider determines what setup/run/inspect
//! mean for each kind of workload (prompt-only, coding-task, web-research,
//! document-analysis, creative-writing, etc.).

use crate::types::Profile;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerifySpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub must_contain: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub must_not_contain: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExpectedSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_cluster_seconds: Option<(u64, u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slow_cluster_seconds: Option<(u64, u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slow_rate: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_count_baseline: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadSpec {
    pub id: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "promptFile")]
    pub prompt_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "sandboxSeed")]
    pub sandbox_seed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifySpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<ExpectedSpec>,
    /// Provider-specific overflow.
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadManifest {
    pub workload: WorkloadSpec,
}

/// A loaded workload manifest, plus where it came from on disk so that
/// providers can resolve relative paths (promptFile, sandboxSeed) correctly.
#[derive(Debug, Clone)]
pub struct LoadedWorkload {
    pub manifest: WorkloadManifest,
    pub manifest_path: std::path::PathBuf,
    pub base_dir: std::path::PathBuf,
    pub source: WorkloadSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadSource {
    Builtin,
    User,
}

#[derive(Debug, Default, Clone)]
pub struct VerifyOutcome {
    pub passed: bool,
    pub details: String,
}

#[derive(Debug, Clone)]
pub struct RunResult {
    pub ok: bool,
    pub duration_ms: u128,
    pub payload_text: Option<String>,
    pub trajectory_path: Option<std::path::PathBuf>,
    pub verify: Option<VerifyOutcome>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct InspectionReport {
    pub run_id: String,
    pub workload_id: String,
    pub walltime_ms: u128,
    pub turns: u32,
    pub compactions: u32,
    pub tokens_before: Vec<u64>,
    pub summary_chars: Vec<u64>,
    pub mode: Option<RunMode>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Fast,
    Slow,
}

/// Trait every workload provider implements. Stored as `Box<dyn WorkloadProvider>`
/// in the registry. Methods are sync; long-running operations are still wrapped
/// in `std::process::Command` calls (which block, but darkmux is a single-task
/// CLI so blocking is fine).
pub trait WorkloadProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn setup(&self, loaded: &LoadedWorkload, run_dir: &Path, sandbox_dir: &Path) -> Result<()>;
    fn run(
        &self,
        loaded: &LoadedWorkload,
        run_dir: &Path,
        sandbox_dir: &Path,
        profile: &Profile,
        profile_name: &str,
    ) -> Result<RunResult>;
    fn inspect(&self, loaded: &LoadedWorkload, run_dir: &Path) -> Result<InspectionReport>;
    fn teardown(&self, _run_dir: &Path, _sandbox_dir: &Path) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workload_manifest_round_trips() {
        let json = r#"{
            "workload": {
                "id": "quick-q",
                "provider": "prompt",
                "description": "A trivial demonstration workload.",
                "agent": "qa",
                "prompt": "What is one observable difference?",
                "verify": {"must_contain": ["active"]}
            }
        }"#;
        let parsed: WorkloadManifest = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.workload.id, "quick-q");
        assert_eq!(parsed.workload.provider, "prompt");
        assert_eq!(parsed.workload.agent.as_deref(), Some("qa"));
        let v = parsed.workload.verify.as_ref().unwrap();
        assert_eq!(v.must_contain, vec!["active".to_string()]);
    }

    #[test]
    fn workload_manifest_rejects_missing_id() {
        let json = r#"{"workload":{"provider":"prompt"}}"#;
        let result: Result<WorkloadManifest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn workload_manifest_extras_captured() {
        let json = r#"{"workload":{"id":"x","provider":"custom","customField":"customValue"}}"#;
        let parsed: WorkloadManifest = serde_json::from_str(json).unwrap();
        let v = parsed
            .workload
            .extras
            .get("customField")
            .and_then(|x| x.as_str())
            .unwrap();
        assert_eq!(v, "customValue");
    }

    #[test]
    fn run_mode_equality() {
        assert_eq!(RunMode::Fast, RunMode::Fast);
        assert_ne!(RunMode::Fast, RunMode::Slow);
    }

    #[test]
    fn workload_source_equality() {
        assert_eq!(WorkloadSource::Builtin, WorkloadSource::Builtin);
        assert_ne!(WorkloadSource::Builtin, WorkloadSource::User);
    }

    #[test]
    fn verify_outcome_default() {
        let v = VerifyOutcome::default();
        assert!(!v.passed);
        assert!(v.details.is_empty());
    }

    #[test]
    fn expected_spec_parses_clusters() {
        let json = r#"{
            "fast_cluster_seconds": [197, 280],
            "slow_cluster_seconds": [600, 950],
            "slow_rate": 0.33,
            "test_count_baseline": 77
        }"#;
        let parsed: ExpectedSpec = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.fast_cluster_seconds, Some((197, 280)));
        assert_eq!(parsed.slow_cluster_seconds, Some((600, 950)));
        assert_eq!(parsed.slow_rate, Some(0.33));
        assert_eq!(parsed.test_count_baseline, Some(77));
    }
}
