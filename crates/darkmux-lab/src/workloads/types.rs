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
    /// (#2833) Minimum LINE coverage percentage the run's verify command
    /// must report, evaluated only for fixtures that also declare
    /// `baseline.test_count` (see `crate::lab::verify_gate`). `None` (the
    /// default — the built-in `pepper-grinder` workload does NOT set
    /// this) means no coverage rule is applied at all, matching today's
    /// behavior exactly.
    ///
    /// darkmux does NOT append a coverage flag to the declared `command`
    /// itself — the workload/fixture author owns the command shape (npm
    /// script, bare `node --test`, a Makefile target, …) and darkmux has no
    /// reliable way to inject `--experimental-test-coverage` into an
    /// arbitrary shell command. If this threshold is set, the declared
    /// `command` MUST itself request coverage (e.g.
    /// `"npm test -- --experimental-test-coverage"` or
    /// `"node --test --experimental-test-coverage test/*.test.js"`). If the
    /// verify output has no coverage table when this is set, that is a gate
    /// FAILURE with an explicit reason — never a silent pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_min_pct: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ExpectedSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_cluster_seconds: Option<(u64, u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slow_cluster_seconds: Option<(u64, u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slow_rate: Option<f32>,
    // (#2833) `test_count_baseline` REMOVED. It was declared here but never
    // read by any code (verified by grep before removal) — the actual
    // source of truth for a coding-task workload's baseline test count is
    // the FIXTURE's own `.fixture.json::baseline.test_count`
    // (`crate::lab::fixture::FixtureManifest`), since the count is a
    // property of the fixture (what "untouched" means), not of the
    // workload (which merely requires a fixture). `crate::lab::verify_gate`
    // reads it from there. Some on-disk workload documents (e.g. the
    // operator's `~/.darkmux/workloads/refresh-rotation.json`) still carry
    // `"expected": {"test_count_baseline": 14}` — that is now a harmless,
    // ignored extra key (config leniency: unknown fields on a struct
    // without `deny_unknown_fields` are silently skipped on read), not a
    // migration hazard.
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
/// `manifest_path` is reserved public-API surface — the existing providers
/// consume `manifest` and `base_dir`; tools that want the resolved path read
/// from this field. `source` is consumed by `lab::run::lab_run`'s per-run
/// banner (#2553) so an operator can tell which tier actually won.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct LoadedWorkload {
    pub manifest: WorkloadManifest,
    pub manifest_path: std::path::PathBuf,
    pub base_dir: std::path::PathBuf,
    pub source: WorkloadSource,
}

/// Which tier a loaded workload actually resolved from — the WINNING tier of
/// the user → on-disk → embedded search order.
///
/// (#2553) Three DISTINCT variants, mirroring
/// `mission_config::MissionConfigSource`. Before this, `OnDisk` and
/// `Embedded` were folded into a single `Builtin` variant — which is exactly
/// why a workload resolving from the shell's cwd was indistinguishable from
/// the binary-embedded one: no field anywhere recorded which document had
/// actually won, so no surface could report it. Splitting the variant is
/// half of closing #2553; the other half is `workloads::load::builtin_dirs`
/// no longer searching cwd at all (see that function's doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkloadSource {
    /// `<user workloads dir>/<id>.json` (or nested `<id>/workload.json`) —
    /// operator override.
    User,
    /// A `templates/builtin/workloads/<id>.json` (or nested form) found on
    /// disk — the explicit `DARKMUX_TEMPLATES_DIR`/`config.dirs.templates`
    /// override, or the `~/.darkmux/templates/...` / `/usr/local/share/...`
    /// candidates. (#2553) NOT the shell's cwd — see
    /// `workloads::load::builtin_dirs`.
    OnDisk,
    /// Compiled into the binary (`EMBEDDED_WORKLOADS`) — always resolvable
    /// even from a bare `cargo install`, no source tree needed.
    Embedded,
}

impl WorkloadSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            WorkloadSource::User => "user",
            WorkloadSource::OnDisk => "on-disk",
            WorkloadSource::Embedded => "embedded",
        }
    }
}

impl std::fmt::Display for WorkloadSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
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
    /// (#2094) Sum of the internal runtime's inter-turn rests for this run,
    /// read from `metrics.json` alongside `turns`/`compactions`. `0` when
    /// the run predates the feature or `metrics.json` is absent — the
    /// runtime's own default-off behavior, not a read failure.
    pub rest_ms: u64,
    pub tokens_before: Vec<u64>,
    pub summary_chars: Vec<u64>,
    pub mode: Option<RunMode>,
    /// (#2494) The workload's own verify outcome, read back from the run
    /// manifest. `None` is a THIRD state, distinct from pass and fail: the
    /// manifest predates schema v4, or the workload declared no verify
    /// command, so nothing was checked. A consumer must not render `None`
    /// as a pass — that conflation is the defect this field closes, where
    /// a run whose tests failed still read green because the only recorded
    /// signal was `ok` (the DISPATCH path's result).
    pub verify: Option<VerifyReport>,
    pub notes: Vec<String>,
}

/// (#2494) The public, manifest-read twin of the crate-internal
/// `VerifyOutcome`. Separate type because `InspectionReport` is public
/// API and `VerifyOutcome` is `pub(crate)`.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub passed: bool,
    pub details: String,
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
        // (#2511) Called AT MOST ONCE, immediately after minting the
        // dispatch session id and BEFORE dispatching — the lab harness
        // (`lab::run`) wires this to `RunLifecycle::set_session_id`, so the
        // run's still-`Running` lifecycle record becomes joinable to its
        // own flow session while the dispatch is still live, not only once
        // `manifest.json` lands at the end. A provider with no single
        // governing dispatch session (`tool-bench` fans out into many, one
        // per task × trial) never calls it — `None` stays the honest
        // answer for that case, not a fabricated representative id.
        on_session_id: &mut dyn FnMut(&str),
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
        assert_eq!(WorkloadSource::OnDisk, WorkloadSource::OnDisk);
        assert_ne!(WorkloadSource::OnDisk, WorkloadSource::User);
        assert_ne!(WorkloadSource::OnDisk, WorkloadSource::Embedded);
        assert_ne!(WorkloadSource::Embedded, WorkloadSource::User);
    }

    #[test]
    fn workload_source_label_and_display_agree() {
        for src in [WorkloadSource::User, WorkloadSource::OnDisk, WorkloadSource::Embedded] {
            assert_eq!(src.label(), src.to_string());
        }
        assert_eq!(WorkloadSource::User.label(), "user");
        assert_eq!(WorkloadSource::OnDisk.label(), "on-disk");
        assert_eq!(WorkloadSource::Embedded.label(), "embedded");
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
            "slow_rate": 0.33
        }"#;
        let parsed: ExpectedSpec = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.fast_cluster_seconds, Some((197, 280)));
        assert_eq!(parsed.slow_cluster_seconds, Some((600, 950)));
        assert_eq!(parsed.slow_rate, Some(0.33));
    }

    /// (#2833) `test_count_baseline` was removed from `ExpectedSpec`. A
    /// still-installed on-disk workload document carrying the old key must
    /// keep parsing (config leniency — unknown fields are silently
    /// ignored), not fail to load.
    #[test]
    fn expected_spec_ignores_retired_test_count_baseline_key() {
        let json = r#"{"slow_rate": 0.5, "test_count_baseline": 14}"#;
        let parsed: ExpectedSpec = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.slow_rate, Some(0.5));
    }

    #[test]
    fn verify_spec_coverage_min_pct_defaults_to_none() {
        let parsed: VerifySpec = serde_json::from_str(r#"{"command": "npm test"}"#).unwrap();
        assert_eq!(parsed.coverage_min_pct, None);
    }

    #[test]
    fn verify_spec_parses_coverage_min_pct() {
        let parsed: VerifySpec =
            serde_json::from_str(r#"{"command": "npm test", "coverage_min_pct": 85.0}"#).unwrap();
        assert_eq!(parsed.coverage_min_pct, Some(85.0));
    }
}
