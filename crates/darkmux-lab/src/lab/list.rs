//! `darkmux lab run list [--limit N]` — list recent runs, most-recent first.
//!
//! Reads run-dirs under .darkmux/runs/, peeks at each manifest.json for
//! workload + duration + ok status, and surfaces a compact table.

use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

/// `run_dir` is public-API surface for downstream tools that want the
/// path alongside the summary (e.g. `darkmux notebook draft` flows).
/// The CLI's table printer doesn't read it, hence the dead-code lint.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub run_id: String,
    pub workload: String,
    pub profile: String,
    pub duration_ms: u128,
    pub ok: bool,
    pub modified: SystemTime,
    pub run_dir: PathBuf,
}

pub fn list_runs(limit: Option<usize>) -> Result<Vec<RunSummary>> {
    // The same resolver lab runs are WRITTEN through — see `lab_dir`'s own
    // docstring on why read and write must not resolve independently.
    let runs_dir = &darkmux_types::config_access::lab_dir();
    if !runs_dir.exists() {
        return Ok(Vec::new());
    }

    let mut summaries: Vec<RunSummary> = Vec::new();

    for entry in fs::read_dir(runs_dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest = path.join("manifest.json");
        if !manifest.exists() {
            continue;
        }
        let Ok(meta) = fs::metadata(&manifest) else { continue };
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let raw = match fs::read_to_string(&manifest) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let parsed: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Prefer the v2 `run_id` field; fall back to the run-dir basename so
        // pre-v2 runs still list. The dir basename is the canonical id that
        // the user passes back to `lab inspect` / `lab compare`.
        let run_id = parsed
            .get("run_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string()
            });
        let workload = parsed
            .get("workload")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let profile = parsed
            .get("profile")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let duration_ms = parsed
            .get("duration_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u128;
        let ok = parsed.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);

        summaries.push(RunSummary {
            run_id,
            workload,
            profile,
            duration_ms,
            ok,
            modified,
            run_dir: path,
        });
    }

    // Most recent first.
    summaries.sort_by_key(|s| std::cmp::Reverse(s.modified));

    if let Some(n) = limit {
        summaries.truncate(n);
    }

    Ok(summaries)
}

/// `runs_dir` is the root that was actually scanned. It is a parameter rather
/// than a hardcoded string because the reader is no longer always
/// `.darkmux/runs` — `lab_dir()` honors `DARKMUX_LAB_DIR` and
/// `config.dirs.lab`, so naming a directory we may not have looked in is how
/// an empty list reads as a bug. Same reasoning `lab_runs_handler` applies on
/// the HTTP side (#1585): report WHY the list is empty, not just that it is.
pub fn format_table(rows: &[RunSummary], runs_dir: &std::path::Path) -> String {
    if rows.is_empty() {
        return format!("(no runs found under {})\n", runs_dir.display());
    }
    // Compute column widths
    let id_w = rows.iter().map(|r| r.run_id.len()).max().unwrap_or(20).max(20);
    let wl_w = rows.iter().map(|r| r.workload.len()).max().unwrap_or(8).max(8);
    let pf_w = rows.iter().map(|r| r.profile.len()).max().unwrap_or(8).max(8);

    let mut out = String::new();
    out.push_str(&format!(
        "{:<id_w$}  {:<wl_w$}  {:<pf_w$}  {:>7}  {:>3}\n",
        "RUN ID", "WORKLOAD", "PROFILE", "WALL", "OK",
        id_w = id_w, wl_w = wl_w, pf_w = pf_w
    ));
    for r in rows {
        out.push_str(&format!(
            "{:<id_w$}  {:<wl_w$}  {:<pf_w$}  {:>6}s  {:>3}\n",
            r.run_id,
            r.workload,
            r.profile,
            r.duration_ms / 1000,
            if r.ok { "✓" } else { "✗" },
            id_w = id_w, wl_w = wl_w, pf_w = pf_w
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// (#2643) `list_runs` reads its scan root through `config_access::
    /// lab_dir()`, which resolves through `paths::resolve(Auto)` — and
    /// `DARKMUX_HOME`, when set, wins over a project-local `./.darkmux`
    /// UNCONDITIONALLY (see `paths::resolve`'s own doc comment; this is
    /// deliberate production behavior, not a bug). The three tests below
    /// used to only `set_current_dir` to a tempdir holding a `.darkmux/`
    /// and rely on the project-local fallback being chosen — which is only
    /// true when `DARKMUX_HOME` happens to be unset in the shell running
    /// `cargo test`. An operator (or CI) with `DARKMUX_HOME` exported
    /// ambiently made `list_runs` scan `<DARKMUX_HOME>/runs` instead of the
    /// fixture's tempdir, silently returning empty — reproduced directly:
    /// `DARKMUX_HOME=/tmp/w24a-scratch-home cargo test -p darkmux-lab --lib
    /// lab::list::tests` failed all three with the exact index-out-of-bounds
    /// / length-0 / missing-"good" panics this comment now guards against.
    ///
    /// The structural fix (per this project's own doctrine: prefer a fix
    /// that removes the hazard over one more test-local guard) is to stop
    /// depending on cwd-relative project-local discovery at all and instead
    /// set `DARKMUX_HOME` explicitly to the SAME root the fixture writes
    /// under — exactly what every real caller (and this task's own
    /// constraints) already does. That makes resolution agree with the
    /// fixture regardless of what the ambient shell exports, and drops the
    /// `set_current_dir` dance (one fewer piece of process-wide state these
    /// tests have to serialize against). RAII (not a raw save/set/restore)
    /// so a panicking assertion still restores the prior value — mirrors
    /// `lab::run::tests::HomeGuard` (same shape, this module's own copy per
    /// this codebase's established per-module-guard convention; see e.g.
    /// `lab/run.rs`, `lab/inspect.rs`, `crawl/plan_step.rs`).
    struct HomeGuard {
        prev: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn set(dir: &std::path::Path) -> Self {
            let prev = std::env::var_os("DARKMUX_HOME");
            unsafe { std::env::set_var("DARKMUX_HOME", dir) };
            Self { prev }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_HOME", v),
                    None => std::env::remove_var("DARKMUX_HOME"),
                }
            }
        }
    }

    fn write_run(runs_dir: &std::path::Path, run_id: &str, workload: &str, dur: u64, ok: bool) {
        let dir = runs_dir.join(run_id);
        fs::create_dir_all(&dir).unwrap();
        let manifest = serde_json::json!({
            "schema_version": 1,
            "session_id": run_id,
            "workload": workload,
            "profile": "test-profile",
            "duration_ms": dur,
            "ok": ok,
        });
        fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
    }

    #[serial_test::serial]
    #[test]
    fn list_empty_when_no_runs_dir() {
        let tmp = TempDir::new().unwrap();
        // (#2643) Same HomeGuard isolation as the tests below — an empty
        // tempdir with no `runs/` under it, rather than cwd + the ambient
        // project/user fallback, so this asserts what its name claims
        // regardless of what `DARKMUX_HOME` happens to be in the shell
        // running `cargo test`.
        let _home = HomeGuard::set(tmp.path());
        let result = list_runs(None).unwrap();
        assert!(result.is_empty(), "no runs/ dir under an isolated DARKMUX_HOME must list empty");
    }

    #[serial_test::serial]
    #[test]
    fn list_returns_runs_in_recency_order() {
        let tmp = TempDir::new().unwrap();
        let darkmux = tmp.path().join(".darkmux");
        let runs_dir = darkmux.join("runs");
        fs::create_dir_all(&runs_dir).unwrap();

        write_run(&runs_dir, "run-old", "wl-a", 100_000, true);
        // ensure older mtime
        std::thread::sleep(std::time::Duration::from_millis(50));
        write_run(&runs_dir, "run-mid", "wl-b", 200_000, true);
        std::thread::sleep(std::time::Duration::from_millis(50));
        write_run(&runs_dir, "run-new", "wl-c", 300_000, false);

        let _home = HomeGuard::set(&darkmux);
        let summaries = list_runs(None).unwrap();

        // Newest first
        assert_eq!(summaries[0].run_id, "run-new");
        assert_eq!(summaries[1].run_id, "run-mid");
        assert_eq!(summaries[2].run_id, "run-old");
        assert_eq!(summaries[0].workload, "wl-c");
        assert!(!summaries[0].ok);
        assert!(summaries[1].ok);
    }

    #[serial_test::serial]
    #[test]
    fn list_respects_limit() {
        let tmp = TempDir::new().unwrap();
        let darkmux = tmp.path().join(".darkmux");
        let runs_dir = darkmux.join("runs");
        fs::create_dir_all(&runs_dir).unwrap();
        for i in 0..7 {
            write_run(&runs_dir, &format!("run-{i}"), "wl", 1000, true);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _home = HomeGuard::set(&darkmux);
        let summaries = list_runs(Some(3)).unwrap();
        assert_eq!(summaries.len(), 3);
        // Most-recent: run-6, run-5, run-4
        assert_eq!(summaries[0].run_id, "run-6");
        assert_eq!(summaries[2].run_id, "run-4");
    }

    #[serial_test::serial]
    #[test]
    fn list_skips_dirs_without_manifest() {
        let tmp = TempDir::new().unwrap();
        let darkmux = tmp.path().join(".darkmux");
        let runs_dir = darkmux.join("runs");
        fs::create_dir_all(&runs_dir).unwrap();
        write_run(&runs_dir, "good", "wl", 1000, true);
        // Bad dir: no manifest.json
        fs::create_dir_all(runs_dir.join("bad")).unwrap();
        fs::write(runs_dir.join("bad/notes.txt"), "no manifest here").unwrap();

        let _home = HomeGuard::set(&darkmux);
        let summaries = list_runs(None).unwrap();
        let names: Vec<&str> = summaries.iter().map(|r| r.run_id.as_str()).collect();
        assert!(names.contains(&"good"));
        assert!(!names.contains(&"bad"));
    }

    #[test]
    fn format_table_renders_header_and_rows() {
        let now = SystemTime::now();
        let rows = vec![
            RunSummary {
                run_id: "abc".into(),
                workload: "long-agentic".into(),
                profile: "deep".into(),
                duration_ms: 198_000,
                ok: true,
                modified: now,
                run_dir: PathBuf::from("/tmp/abc"),
            },
            RunSummary {
                run_id: "def".into(),
                workload: "bounded-todo".into(),
                profile: "fast".into(),
                duration_ms: 60_000,
                ok: false,
                modified: now,
                run_dir: PathBuf::from("/tmp/def"),
            },
        ];
        let out = format_table(&rows, std::path::Path::new("/tmp/x/runs"));
        assert!(out.contains("RUN ID"));
        assert!(out.contains("abc"));
        assert!(out.contains("def"));
        assert!(out.contains("198s"));
        assert!(out.contains("60s"));
    }

    #[test]
    fn format_table_handles_empty() {
        let out = format_table(&[], std::path::Path::new("/tmp/x/runs"));
        assert!(out.contains("no runs"));
    }
}
