//! WorkloadProvider trait + manifest schema.
//!
//! A workload is **any prompt + optional setup + measurement criteria**.
//! Not just coding tasks. The provider determines what setup/run/inspect
//! mean for each kind of workload (prompt-only, coding-task, web-research,
//! document-analysis, creative-writing, etc.).

use darkmux_types::Profile;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct VerifySpec {
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
pub(crate) struct ExpectedSpec {
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
pub(crate) struct WorkloadSpec {
    pub id: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Darkmux role manifest id to dispatch the workload through.
    /// Looks up `templates/builtin/roles/<role>.json` for the system
    /// prompt + tool palette. Beat 36 directional principle: DM's
    /// concepts are primary — workloads reference DM roles, not OC
    /// agent personas.
    ///
    /// When `None`, providers fall back to a generic system prompt
    /// (today: `code-reviewer` as the default for prompt-shape
    /// workloads, since it's the role best-suited to single-turn
    /// QA-flavored tasks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "promptFile"
    )]
    pub prompt_file: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "sandboxSeed"
    )]
    pub sandbox_seed: Option<String>,
    /// Inline sandbox files shipped with the workload manifest itself
    /// — keys are paths relative to the sandbox dir, values are full
    /// file contents. The coding-task provider writes each pair into
    /// the sandbox before dispatch. Lets a workload bring a complete
    /// runnable scaffold without requiring an external project on
    /// disk; works with embedded workloads (unlike `sandboxSeed`,
    /// which needs a sibling directory). Example: a small Python
    /// file + a unittest that pairs with it.
    ///
    /// **Precedence with `sandboxSeed`**: when both are present, the
    /// seed copy runs first and `setupContent` overlays on top — keys
    /// in `setupContent` overwrite same-named files from the seed.
    /// Lets an embedded workload patch a specific file over a copied
    /// external seed.
    ///
    /// **Re-application**: applied on every dispatch (not skip-if-
    /// exists). The sandbox is operator-mutated by each run (the
    /// agent edits files); re-applying gives every dispatch a
    /// deterministic starting point.
    ///
    /// **Path safety**: keys MUST be relative paths under the sandbox.
    /// Absolute paths, `..` components, and Windows drive prefixes are
    /// rejected at setup time by the coding-task provider — prevents
    /// untrusted operator-installed workload manifests from writing
    /// outside the sandbox.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        rename = "setupContent"
    )]
    pub setup_content: BTreeMap<String, String>,
    /// Marks workloads that depend on an external sandbox (a Node
    /// project, a real repo checkout) that the operator must provide.
    /// When true, the coding-task provider checks at setup time
    /// whether the sandbox dir is empty AND no inline `setupContent`
    /// is present; if so, bails loud with an operator-actionable
    /// hint instead of dispatching against an empty workspace.
    #[serde(
        default,
        skip_serializing_if = "std::ops::Not::not",
        rename = "requiresExternalSandbox"
    )]
    pub requires_external_sandbox: bool,
    /// (#490) Declares which abstract fixture definition this workload
    /// needs at dispatch time. Format: `<name>@<version>` matched
    /// LITERALLY (e.g. `"node-refresh-token-rotation@1.0"`) — the version
    /// is an exact string, NOT a semver range. `@>=1.0`-style operators
    /// are not supported and the resolver rejects them loudly (semver
    /// support tracked in #496). When set, the lab resolver consults
    /// `~/.darkmux/lab-registry.json` for a fixture whose
    /// `.fixture.json::satisfies` equals this string; COW-clones it as
    /// the per-run sandbox source.
    ///
    /// When unset, falls back to the default sandbox path
    /// `{paths.sandboxes}/<workload-id>/` (the current convention for
    /// workloads with `setupContent` or no external dependency).
    ///
    /// Replaces the pre-#490 `DARKMUX_SANDBOX_<WORKLOAD-ID>` env-var
    /// resolution. Per the `no_compat_baggage_pre_1_0` doctrine, no
    /// env-var fallback ships.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_fixture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifySpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<ExpectedSpec>,
    /// (#703 Slice 4) Docker image this workload should dispatch into. When
    /// set, darkmux injects its runtime binary into this image so the agent
    /// can compile/test the workload in-sandbox (e.g. `"rust:slim"` for a
    /// Rust fixture). `None` → the default slim runtime image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Provider-specific overflow.
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkloadManifest {
    pub workload: WorkloadSpec,
}

/// A loaded workload manifest, plus where it came from on disk so that
/// providers can resolve relative paths (promptFile, sandboxSeed) correctly.
///
/// `manifest_path` and `source` are reserved public-API surface — the
/// existing providers consume `manifest` and `base_dir`; tools that
/// want provenance metadata read from these. The dead-code lint sees
/// no current callers.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct LoadedWorkload {
    pub manifest: WorkloadManifest,
    pub manifest_path: std::path::PathBuf,
    pub base_dir: std::path::PathBuf,
    pub source: WorkloadSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkloadSource {
    Builtin,
    User,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct VerifyOutcome {
    pub passed: bool,
    pub details: String,
}

/// `payload_text` and `trajectory_path` are public-API surface for
/// downstream consumers (notebook drafting reads them); the CLI's run
/// summary doesn't, hence the dead-code lint.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct RunResult {
    pub ok: bool,
    pub duration_ms: u128,
    pub payload_text: Option<String>,
    pub trajectory_path: Option<std::path::PathBuf>,
    pub verify: Option<VerifyOutcome>,
    pub error: Option<String>,
}

/// `summary_chars` is the per-turn summary-length series collected
/// during inspection. Public-API surface for tools that want to plot
/// summary-size dynamics; the CLI's inspect summary doesn't use it
/// directly, hence the dead-code lint.
#[allow(dead_code)]
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
/// `description` and `teardown` are part of the trait's public surface.
/// Implementations provide them (description for tooling, teardown
/// optional with a default impl), but no current call site consumes
/// them via dynamic dispatch — hence the dead-code lint. Keeping the
/// trait shape stable for the `lab providers` subcommand + future
/// per-workload cleanup needs.
#[allow(dead_code)]
pub(crate) trait WorkloadProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn setup(&self, loaded: &LoadedWorkload, run_dir: &Path, sandbox_dir: &Path) -> Result<()>;
    /// Run the workload through darkmux's in-house container-bounded
    /// runtime, the only dispatch path (#1405). The former `runtime`
    /// parameter (a single-variant enum) retired in #1426 ship-3 along with
    /// the enum itself; there is only one runtime, so the trait no longer
    /// threads it.
    ///
    /// Argument count exceeds clippy's default threshold; the trait shape
    /// mirrors the dispatch contract closely (the profile is inherent to a
    /// dispatch). A `RunContext` struct is a candidate cleanup but out of
    /// scope here.
    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        loaded: &LoadedWorkload,
        run_dir: &Path,
        sandbox_dir: &Path,
        profile: &Profile,
        profile_name: &str,
        // (#984) The `--profiles-file` the dispatch's model + context-window
        // resolution must load from, so a lab `--profiles-file` actually
        // reaches the dispatch (not just lab run's own profile lookup).
        config_path: Option<&str>,
        // (#986) Per-run compaction overrides for the loop lab. When
        // `Some`, the fields override what `CompactionDispatchArgs::
        // from_profile` derived (the loop-variation axis); `None` (the
        // `lab run` path) leaves the profile's compaction config intact.
        loop_override: Option<&crate::lab::loop_report::LoopCompactionOverride>,
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
                "role": "code-reviewer",
                "prompt": "What is one observable difference?",
                "verify": {"must_contain": ["active"]}
            }
        }"#;
        let parsed: WorkloadManifest = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.workload.id, "quick-q");
        assert_eq!(parsed.workload.provider, "prompt");
        assert_eq!(parsed.workload.role.as_deref(), Some("code-reviewer"));
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
