//! `darkmux lab inspect <run-id-or-path>` — analyze a run via its provider.

use crate::lab::paths::{self, ResolveScope};
use crate::workloads::load::load;
use crate::workloads::registry::with_provider;
use crate::workloads::types::InspectionReport;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// A lab run directory's trajectory, folded: THE way every lab reader
/// (`stats`, `inspect`, `loop`, the compaction-summary view) reads a
/// recorded run's turns, tokens, rests and detector firings.
///
/// The trajectory is `<run>/trajectory.jsonl`, copied there when the run
/// finished (#364). A run recorded before that copy existed has it only in
/// its sandbox, at `<sandbox>/.darkmux-runtime/trajectory.jsonl`, the
/// sandbox its `manifest.json` names; that is the one legacy location, read
/// only when the run's own copy is absent. A run directory's `metrics.json`
/// (written until 4.0) is never read: its numbers are all in the trajectory,
/// and where the two disagreed the file was the one that was wrong.
///
/// The sandbox is model-writable, so the legacy read goes through the
/// contained out-dir reader (#2869): a planted symlink or FIFO there is
/// refused, not followed. The run's own copy is host-written.
pub fn run_trajectory(run_dir: &Path) -> darkmux_trajectory::TrajectoryFold {
    let own = run_dir.join(darkmux_trajectory::TRAJECTORY_FILE);
    if own.exists() {
        return darkmux_trajectory::TrajectoryFold::from_path(&own);
    }
    legacy_sandbox(run_dir)
        .map(|sandbox| darkmux_crew::dispatch_internal::out_dir_trajectory(&sandbox))
        .unwrap_or_default()
}

/// The sandbox a run's `manifest.json` names, if any.
fn legacy_sandbox(run_dir: &Path) -> Option<PathBuf> {
    fs::read_to_string(run_dir.join("manifest.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|m| m.get("sandbox").and_then(Value::as_str).map(PathBuf::from))
}

/// The distinct compaction summaries a run's trajectory recorded (the
/// retired openclaw runtime wrote its compactions only as summary messages
/// inside the thread; see `darkmux_trajectory::legacy`). Empty for a run
/// with no trajectory, or none of that shape.
pub fn read_compaction_summaries(run_dir: &Path) -> Vec<darkmux_trajectory::legacy::LegacyCompaction> {
    run_trajectory(run_dir).legacy.compactions
}

pub fn resolve_run_path(run_path: &str) -> PathBuf {
    resolve_run_dir(run_path)
}

/// (#2986) A run with a lifecycle record but no manifest ended before its
/// provider finished (it errored, or was interrupted or killed): report what
/// the lifecycle recorded, its status and its error, instead of failing.
fn report_from_lifecycle(rec: &crate::lab::lifecycle::LifecycleRecord) -> InspectionReport {
    let status = serde_json::to_value(rec.status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let mut notes = vec![
        "the run ended before its provider finished, so it has no manifest".to_string(),
        format!("status: {status}"),
    ];
    if let Some(e) = &rec.error {
        notes.push(format!("error: {e}"));
    }
    InspectionReport {
        run_id: rec.run_id.clone(),
        workload_id: rec.workload.clone(),
        walltime_ms: rec.ended_at_ms.map_or(0, |end| end.saturating_sub(rec.started_at_ms)) as u128,
        notes,
        ..Default::default()
    }
}

pub fn lab_inspect(run_path: &str) -> Result<InspectionReport> {
    let run_dir = resolve_run_dir(run_path);
    let manifest_path = run_dir.join("manifest.json");
    if !manifest_path.exists() {
        if let Some(rec) = crate::lab::lifecycle::read(&run_dir) {
            return Ok(report_from_lifecycle(&rec));
        }
        bail!(
            "no run manifest at {} — was this dispatched via `darkmux lab run`?",
            manifest_path.display()
        );
    }
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let meta: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    let workload_id = meta
        .get("workload")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("manifest missing 'workload' field"))?;
    let provider_id = meta
        .get("provider")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("manifest missing 'provider' field"))?;

    // The workload document resolves at the darkmux root, like `lab_run` and
    // `lab_workloads`.
    let user_workloads_root = paths::resolve(ResolveScope::ForceUser).root;
    let loaded = load(workload_id, Some(&user_workloads_root))?;

    let report = with_provider(provider_id, |p| p.inspect(&loaded, &run_dir))??;
    Ok(report)
}

/// A run named by a path (it contains a `/`) is that path; a bare id is a run
/// under the lab dir, never a same-named directory in the cwd. `lab_dir()`,
/// not a second resolution of the lab root: inspect must look where `lab run`
/// actually wrote (#1882).
pub(crate) fn resolve_run_dir(path: &str) -> PathBuf {
    if path.contains('/') {
        return PathBuf::from(path);
    }
    darkmux_types::config_access::lab_dir().join(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// RAII guard that changes the process cwd for the test's duration and
    /// restores it on drop — mirrors `workloads::load`'s and `lab::run`'s
    /// test-only `CwdGuard` (#2432/#2553/#2590). Every caller MUST be
    /// `#[serial_test::serial]` — cwd is a process-global resource.
    struct CwdGuard {
        prev: PathBuf,
    }

    impl CwdGuard {
        fn new(dir: &Path) -> Self {
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir).unwrap();
            Self { prev }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.prev);
        }
    }

    /// RAII guard that scopes the REAL `HOME` env var (which
    /// `dirs::home_dir()` reads) and force-clears `DARKMUX_HOME` for the
    /// duration, restoring both on drop. Mirrors `lab::run::tests::RealHomeGuard`
    /// — `DARKMUX_HOME` short-circuits `paths::resolve` before the
    /// `Auto`/`ForceUser` distinction under test is ever evaluated, so a
    /// test exercising that distinction must move `HOME` instead and clear
    /// any ambient `DARKMUX_HOME` in the shell running `cargo test`.
    struct RealHomeGuard {
        prev_home: Option<std::ffi::OsString>,
        prev_darkmux_home: Option<std::ffi::OsString>,
    }

    impl RealHomeGuard {
        fn set(dir: &Path) -> Self {
            let prev_home = std::env::var_os("HOME");
            let prev_darkmux_home = std::env::var_os("DARKMUX_HOME");
            unsafe {
                std::env::set_var("HOME", dir);
                std::env::remove_var("DARKMUX_HOME");
            }
            Self {
                prev_home,
                prev_darkmux_home,
            }
        }
    }

    impl Drop for RealHomeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_home {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
                match &self.prev_darkmux_home {
                    Some(v) => std::env::set_var("DARKMUX_HOME", v),
                    None => std::env::remove_var("DARKMUX_HOME"),
                }
            }
        }
    }

    /// (#2590) The third document-lookup site, missed by the original fix:
    /// `lab_inspect` must not resolve a workload id that exists ONLY in a
    /// cwd-local `.darkmux/workloads/`, matching `lab_run`/`lab_workloads`'s
    /// `ForceUser` fix. Red-proved: reverting `lab_inspect`'s
    /// `ResolveScope::ForceUser` (the workload-user-dir resolution, not
    /// `resolve_run_dir`'s cwd-sensitive run-artifact lookup, which is
    /// untouched and correct) back to `Auto` makes this resolve the cwd
    /// document and proceed to `with_provider` instead of erroring
    /// "not found".
    #[serial_test::serial]
    #[test]
    fn lab_inspect_ignores_a_cwd_local_darkmux_dir_for_workload_lookup() {
        let project = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".darkmux").join("workloads")).unwrap();
        std::fs::write(
            project
                .path()
                .join(".darkmux")
                .join("workloads")
                .join("cwd-only-ghost.json"),
            r#"{"workload":{"id":"cwd-only-ghost","provider":"prompt","prompt":"hi"}}"#,
        )
        .unwrap();

        // A real run dir naming that cwd-only-only id in its manifest, so
        // `lab_inspect` gets past the manifest-exists check and reaches the
        // workload-document lookup under test.
        let run_dir = TempDir::new().unwrap();
        std::fs::write(
            run_dir.path().join("manifest.json"),
            r#"{"workload":"cwd-only-ghost","provider":"prompt"}"#,
        )
        .unwrap();

        // An empty, isolated home — nothing here defines `cwd-only-ghost`
        // either, so the ONLY way it could resolve is via the cwd.
        let home = TempDir::new().unwrap();
        let _home_guard = RealHomeGuard::set(home.path());
        let _cwd_guard = CwdGuard::new(project.path());

        let err = lab_inspect(run_dir.path().to_str().unwrap()).unwrap_err();

        assert!(
            err.to_string().contains("not found"),
            "lab_inspect must not resolve a cwd-only workload id when \
             inspecting a run — the user tier is forced home (#2590); \
             got: {err}"
        );
    }

    #[test]
    fn resolve_run_dir_absolute() {
        let p = resolve_run_dir("/tmp/some/run");
        assert_eq!(p, PathBuf::from("/tmp/some/run"));
    }

    #[test]
    fn resolve_run_dir_relative() {
        let p = resolve_run_dir("./local/run");
        assert_eq!(p, PathBuf::from("./local/run"));
    }

    #[test]
    fn resolve_run_dir_id_resolves_under_the_lab_dir_even_when_missing() {
        let p = resolve_run_dir("just-an-id");
        // An id that doesn't exist under the lab dir still resolves there:
        // the existence check happens later in lab_inspect, and its error
        // names the place that was looked in.
        assert_eq!(p, darkmux_types::config_access::lab_dir().join("just-an-id"));
    }

    #[test]
    fn lab_inspect_errors_on_missing_manifest() {
        let tmp = TempDir::new().unwrap();
        let err = lab_inspect(tmp.path().to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("no run manifest"));
    }

    #[test]
    fn read_compaction_summaries_empty_when_no_trajectory() {
        let tmp = TempDir::new().unwrap();
        let summaries = read_compaction_summaries(tmp.path());
        assert!(summaries.is_empty());
    }

    #[test]
    fn read_compaction_summaries_extracts_unique_summaries() {
        let tmp = TempDir::new().unwrap();
        // Two prompt.submitted events; the second has a compactionSummary.
        // The third repeats the same summary and should be deduped.
        let traj = r#"{"type":"prompt.submitted","data":{"messages":[]}}
{"type":"prompt.submitted","data":{"messages":[{"role":"compactionSummary","summary":"alpha summary content here","tokensBefore":48000}]}}
{"type":"prompt.submitted","data":{"messages":[{"role":"compactionSummary","summary":"alpha summary content here","tokensBefore":52000}]}}
{"type":"prompt.submitted","data":{"messages":[{"role":"compactionSummary","summary":"beta summary newer","tokensBefore":60000}]}}
"#;
        std::fs::write(tmp.path().join("trajectory.jsonl"), traj).unwrap();
        let summaries = read_compaction_summaries(tmp.path());
        // Two unique summaries (alpha + beta), even though alpha repeats
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].tokens_before, 48000);
        assert_eq!(summaries[1].tokens_before, 60000);
        assert!(summaries[0].summary.contains("alpha"));
        assert!(summaries[1].summary.contains("beta"));
        assert_eq!((summaries[0].turn, summaries[1].turn), (2, 4), "the turn that first carried each");
    }

    /// The one legacy location: a run recorded before its trajectory was
    /// copied into the run directory (#364) is read from the sandbox its
    /// manifest names. The run's own copy wins when both exist. A
    /// `metrics.json` in the run directory is never read.
    #[test]
    fn a_run_trajectory_is_its_own_copy_else_the_sandbox_its_manifest_names() {
        let run = TempDir::new().unwrap();
        let sandbox = TempDir::new().unwrap();
        let rt = sandbox.path().join(".darkmux-runtime");
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::write(rt.join("trajectory.jsonl"), "{\"type\":\"model.completed\",\"seq\":1}\n{\"type\":\"model.completed\",\"seq\":2}\n").unwrap();
        std::fs::write(
            run.path().join("manifest.json"),
            serde_json::json!({ "sandbox": sandbox.path() }).to_string(),
        )
        .unwrap();
        std::fs::write(run.path().join("metrics.json"), r#"{"turns":9,"total_prompt_tokens":175557}"#).unwrap();
        assert_eq!(run_trajectory(run.path()).turns(), 2, "the sandbox copy, not metrics.json");

        std::fs::write(run.path().join("trajectory.jsonl"), "{\"type\":\"model.completed\",\"seq\":1}\n").unwrap();
        assert_eq!(run_trajectory(run.path()).turns(), 1, "the run's own copy wins");
    }

    /// (#2869) The sandbox is model-writable: a legacy sandbox trajectory
    /// that is a symlink to a host file is refused, not folded.
    #[test]
    fn a_symlinked_legacy_sandbox_trajectory_is_refused() {
        let run = TempDir::new().unwrap();
        let sandbox = TempDir::new().unwrap();
        let rt = sandbox.path().join(".darkmux-runtime");
        std::fs::create_dir_all(&rt).unwrap();
        let host = sandbox.path().join("host-trajectory.jsonl");
        std::fs::write(&host, "{\"type\":\"model.completed\",\"seq\":1}\n").unwrap();
        std::os::unix::fs::symlink(&host, rt.join("trajectory.jsonl")).unwrap();
        std::fs::write(
            run.path().join("manifest.json"),
            serde_json::json!({ "sandbox": sandbox.path() }).to_string(),
        )
        .unwrap();
        assert_eq!(run_trajectory(run.path()).turns(), 0, "the symlinked host file was folded");
    }

    #[test]
    fn read_compaction_summaries_skips_empty_summaries() {
        let tmp = TempDir::new().unwrap();
        let traj = r#"{"type":"prompt.submitted","data":{"messages":[{"role":"compactionSummary","summary":"","tokensBefore":1000}]}}
"#;
        std::fs::write(tmp.path().join("trajectory.jsonl"), traj).unwrap();
        let summaries = read_compaction_summaries(tmp.path());
        assert!(summaries.is_empty());
    }
}
