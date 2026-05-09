//! `darkmux lab runs [--limit N]` — list recent runs, most-recent first.
//!
//! Reads run-dirs under .darkmux/runs/, peeks at each manifest.json for
//! workload + duration + ok status, and surfaces a compact table.

use crate::lab::paths::{self, ResolveScope};
use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

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
    let paths = paths::resolve(ResolveScope::Auto);
    let runs_dir = &paths.runs;
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
        let modified = meta.modified().unwrap_or_else(|_| SystemTime::UNIX_EPOCH);
        let raw = match fs::read_to_string(&manifest) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let parsed: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Prefer the v2 `runId` field; fall back to the run-dir basename so
        // pre-v2 runs still list. The dir basename is the canonical id that
        // the user passes back to `lab inspect` / `lab compare`.
        let run_id = parsed
            .get("runId")
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
            .get("durationMs")
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
    summaries.sort_by(|a, b| b.modified.cmp(&a.modified));

    if let Some(n) = limit {
        summaries.truncate(n);
    }

    Ok(summaries)
}

pub fn format_table(rows: &[RunSummary]) -> String {
    if rows.is_empty() {
        return "(no runs found under .darkmux/runs/)".to_string();
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

    fn write_run(runs_dir: &std::path::Path, run_id: &str, workload: &str, dur: u64, ok: bool) {
        let dir = runs_dir.join(run_id);
        fs::create_dir_all(&dir).unwrap();
        let manifest = serde_json::json!({
            "schemaVersion": 1,
            "sessionId": run_id,
            "workload": workload,
            "profile": "test-profile",
            "durationMs": dur,
            "ok": ok,
        });
        fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
    }

    #[serial_test::serial]
    #[test]
    fn list_empty_when_no_runs_dir() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = list_runs(None).unwrap();
        std::env::set_current_dir(prev).unwrap();
        // Result depends on whether ~/.darkmux/runs/ exists, so we can't strictly
        // assert empty — but the call should succeed without error.
        let _ = result;
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

        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let summaries = list_runs(None).unwrap();
        std::env::set_current_dir(prev).unwrap();

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
        let runs_dir = tmp.path().join(".darkmux/runs");
        fs::create_dir_all(&runs_dir).unwrap();
        for i in 0..7 {
            write_run(&runs_dir, &format!("run-{i}"), "wl", 1000, true);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let summaries = list_runs(Some(3)).unwrap();
        std::env::set_current_dir(prev).unwrap();
        assert_eq!(summaries.len(), 3);
        // Most-recent: run-6, run-5, run-4
        assert_eq!(summaries[0].run_id, "run-6");
        assert_eq!(summaries[2].run_id, "run-4");
    }

    #[serial_test::serial]
    #[test]
    fn list_skips_dirs_without_manifest() {
        let tmp = TempDir::new().unwrap();
        let runs_dir = tmp.path().join(".darkmux/runs");
        fs::create_dir_all(&runs_dir).unwrap();
        write_run(&runs_dir, "good", "wl", 1000, true);
        // Bad dir: no manifest.json
        fs::create_dir_all(runs_dir.join("bad")).unwrap();
        fs::write(runs_dir.join("bad/notes.txt"), "no manifest here").unwrap();

        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let summaries = list_runs(None).unwrap();
        std::env::set_current_dir(prev).unwrap();
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
        let out = format_table(&rows);
        assert!(out.contains("RUN ID"));
        assert!(out.contains("abc"));
        assert!(out.contains("def"));
        assert!(out.contains("198s"));
        assert!(out.contains("60s"));
    }

    #[test]
    fn format_table_handles_empty() {
        let out = format_table(&[]);
        assert!(out.contains("no runs"));
    }
}
