//! `darkmux lab inspect <run-id-or-path>` — analyze a run via its provider.

use crate::lab::paths::{self, ResolveScope};
use crate::workloads::load::load;
use crate::workloads::registry::with_provider;
use crate::workloads::types::InspectionReport;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct CompactionSummary {
    pub turn_index: usize,
    pub tokens_before: u64,
    pub summary_chars: usize,
    pub summary_text: String,
}

/// Read the trajectory file for a run and extract every unique
/// `compactionSummary` message. Each entry includes the raw summary text
/// the compaction model wrote, the LMStudio-reported tokensBefore, and the
/// turn index where the summary first appeared.
///
/// Returns an empty vec if no trajectory.jsonl exists (e.g. for prompt
/// provider runs which don't snapshot a trajectory). The caller can use
/// that to render "(no trajectory recorded)" rather than an error.
pub fn read_compaction_summaries(run_dir: &Path) -> Result<Vec<CompactionSummary>> {
    let traj = run_dir.join("trajectory.jsonl");
    if !traj.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&traj)
        .with_context(|| format!("reading {}", traj.display()))?;
    let mut out: Vec<CompactionSummary> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut turn_idx = 0usize;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else { continue };
        if ev.get("type").and_then(|t| t.as_str()) != Some("prompt.submitted") {
            continue;
        }
        turn_idx += 1;
        let Some(msgs) = ev.get("data").and_then(|d| d.get("messages")).and_then(|m| m.as_array()) else {
            continue;
        };
        for m in msgs {
            if m.get("role").and_then(|r| r.as_str()) != Some("compactionSummary") {
                continue;
            }
            let summary_text = m
                .get("summary")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            if summary_text.is_empty() {
                continue;
            }
            // Dedup by 80-char prefix — same summary persists across turns
            // until the next compaction overrides it.
            let key: String = summary_text.chars().take(80).collect();
            if !seen.insert(key) {
                continue;
            }
            out.push(CompactionSummary {
                turn_index: turn_idx,
                tokens_before: m.get("tokensBefore").and_then(|v| v.as_u64()).unwrap_or(0),
                // (#906) char count, not byte count — the field is named
                // `summary_chars` and multi-byte summaries would otherwise
                // over-report.
                summary_chars: summary_text.chars().count(),
                summary_text,
            });
        }
    }
    Ok(out)
}

pub fn resolve_run_path(run_path: &str) -> PathBuf {
    resolve_run_dir(run_path)
}

pub fn lab_inspect(run_path: &str) -> Result<InspectionReport> {
    let run_dir = resolve_run_dir(run_path);
    let manifest_path = run_dir.join("manifest.json");
    if !manifest_path.exists() {
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

    // (#2590) This is the THIRD workload-document lookup site, missed by the
    // original fix, which covered only `lab::run::lab_run` and
    // `lab::run::lab_workloads`. `resolve_run_dir` above deliberately stays
    // `Auto`/project-sensitive (it reads `config_access::lab_dir()`, the same
    // cwd-sensitive root `lab run` wrote the run's artifacts under — that
    // part is correct and untouched). The workload DOCUMENT lookup below is
    // a separate concern: before this fix it also used `Auto`'s root, so a
    // `./.darkmux/workloads/<id>.json` sitting in the shell's cwd could
    // shadow the embedded/home-tier document of the same id when inspecting
    // a run, and a run naming a home-tier-only workload could fail "not
    // found" here even though `lab run`/`lab workload list` resolve it fine
    // — the same split-tier inconsistency `lab_run`/`lab_workloads` closed,
    // one call site over. Force the workload user tier home, matching those
    // two.
    let user_workloads_root = paths::resolve(ResolveScope::ForceUser).root;
    let loaded = load(workload_id, Some(&user_workloads_root))?;

    let report = with_provider(provider_id, |p| p.inspect(&loaded, &run_dir))??;
    Ok(report)
}

fn resolve_run_dir(path: &str) -> PathBuf {
    if path.starts_with('/') || path.starts_with("./") || path.starts_with("../") || path.contains('/') {
        return PathBuf::from(path);
    }
    // `lab_dir()`, not a second resolution of the runs root — inspect must look
    // where `lab run` actually wrote (#1882).
    let candidate = darkmux_types::config_access::lab_dir().join(path);
    if candidate.exists() {
        return candidate;
    }
    PathBuf::from(path)
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
    fn resolve_run_dir_id_falls_back_when_missing() {
        let p = resolve_run_dir("just-an-id");
        // When the id doesn't exist under runs/, we return it as-is.
        // The actual existence check happens later in lab_inspect.
        assert!(p.to_str().unwrap().ends_with("just-an-id"));
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
        let summaries = read_compaction_summaries(tmp.path()).unwrap();
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
        let summaries = read_compaction_summaries(tmp.path()).unwrap();
        // Two unique summaries (alpha + beta), even though alpha repeats
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].tokens_before, 48000);
        assert_eq!(summaries[1].tokens_before, 60000);
        assert!(summaries[0].summary_text.contains("alpha"));
        assert!(summaries[1].summary_text.contains("beta"));
    }

    #[test]
    fn read_compaction_summaries_skips_empty_summaries() {
        let tmp = TempDir::new().unwrap();
        let traj = r#"{"type":"prompt.submitted","data":{"messages":[{"role":"compactionSummary","summary":"","tokensBefore":1000}]}}
"#;
        std::fs::write(tmp.path().join("trajectory.jsonl"), traj).unwrap();
        let summaries = read_compaction_summaries(tmp.path()).unwrap();
        assert!(summaries.is_empty());
    }
}
