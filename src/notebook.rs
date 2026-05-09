//! `darkmux notebook draft <run-id>` — agent-as-scribe for lab notebook entries.
//!
//! Reads a lab run's manifest + trajectory, builds a prompt asking the active
//! runtime to draft a lab-notebook entry, dispatches it (typically against the
//! `scribe` profile), and writes the draft to .darkmux/notebook/<date>-<slug>.md.

use crate::lab::paths::{self, ResolveScope};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct DraftOptions {
    pub run_id: String,
    pub agent: String,
    pub slug: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug)]
pub struct DraftReport {
    pub run_dir: PathBuf,
    pub entry_path: PathBuf,
    pub prompt_chars: usize,
    pub reply_chars: usize,
}

const NOTEBOOK_PROMPT_TEMPLATE: &str = r#"You are drafting an entry in a lab notebook in the style of an electrical-engineering experiment log.

Aesthetic: scientific but not dry. Date stamps. "OBSERVATION:" / "RESULT:" / "EUREKA:" markers. Tables OK but keep formatting simple. Notes that matter; no editorial flourishes. Mid-experiment jotting acceptable.

You are given a single run's data below — its manifest, key trajectory metrics, and a one-line summary. Author a notebook entry that captures:

- DATE + a one-line headline
- WHAT WAS RUN (workload, profile, command line)
- RESULT (wall clock, turns, compactions, mode classification, ok/fail)
- OBSERVATION (1-3 bullets of what's mechanically interesting in this run — e.g., compaction fired at X tokens, or single-turn windfall, or verify failed because Y)
- NEXT (optional — what would be worth running next given this result)

Be concise. ~150-300 words. Use simple markdown. No introductions or conclusions; readers know they're reading a lab notebook.

DO NOT include any reference to specific people, companies, or non-public projects beyond what's literally in the run data below.

---

# Run data

{run_data}
"#;

pub fn draft_entry(opts: &DraftOptions) -> Result<DraftReport> {
    let paths = paths::resolve(ResolveScope::Auto);
    paths::ensure(&paths)?;

    // Resolve the run dir from the given id.
    let run_dir = if Path::new(&opts.run_id).is_absolute() || opts.run_id.contains('/') {
        PathBuf::from(&opts.run_id)
    } else {
        paths.runs.join(&opts.run_id)
    };
    if !run_dir.exists() {
        bail!("run dir not found: {}", run_dir.display());
    }

    let manifest_path = run_dir.join("manifest.json");
    if !manifest_path.exists() {
        bail!(
            "no manifest.json in run dir {} — was this dispatched via 'darkmux lab run'?",
            run_dir.display()
        );
    }
    let manifest_raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let manifest: Value = serde_json::from_str(&manifest_raw)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;

    let run_data = build_run_data_summary(&run_dir, &manifest)?;
    let prompt = NOTEBOOK_PROMPT_TEMPLATE.replace("{run_data}", &run_data);

    // Dispatch to the runtime.
    let runtime_cmd = env::var("DARKMUX_RUNTIME_CMD").unwrap_or_else(|_| "openclaw".to_string());
    let session_id = format!(
        "darkmux-notebook-{}-{}",
        opts.run_id,
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );

    let entry_text = if opts.dry_run {
        format!("[DRY RUN — would have dispatched the prompt below to '{runtime_cmd} agent --agent {} --message ...']\n\n{prompt}", opts.agent)
    } else {
        let output = Command::new(&runtime_cmd)
            .args([
                "agent",
                "--agent",
                &opts.agent,
                "--session-id",
                &session_id,
                "--json",
                "--timeout",
                "1800",
                "--message",
                &prompt,
            ])
            .output()
            .with_context(|| format!("running `{runtime_cmd} agent ...`"))?;
        if !output.status.success() {
            bail!(
                "agent dispatch failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        extract_reply_text(&stdout)
    };

    // Compose the entry path: <date>-<slug>.md
    let date = chrono_like_today();
    let slug = opts
        .slug
        .clone()
        .unwrap_or_else(|| slugify(&format!(
            "{}-{}",
            manifest
                .get("workload")
                .and_then(|v| v.as_str())
                .unwrap_or("run"),
            shortid(&opts.run_id)
        )));
    let entry_path = paths.notebook.join(format!("{date}-{slug}.md"));

    if !opts.dry_run {
        if !paths.notebook.exists() {
            fs::create_dir_all(&paths.notebook)?;
        }
        let header = format!(
            "<!-- darkmux:notebook-entry: drafted from run {} on {} via 'darkmux notebook draft' -->\n\n",
            opts.run_id, date
        );
        fs::write(&entry_path, format!("{header}{entry_text}\n"))
            .with_context(|| format!("writing {}", entry_path.display()))?;
    }

    Ok(DraftReport {
        run_dir,
        entry_path,
        prompt_chars: prompt.len(),
        reply_chars: entry_text.len(),
    })
}

fn build_run_data_summary(run_dir: &Path, manifest: &Value) -> Result<String> {
    let workload = manifest
        .get("workload")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let provider = manifest
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let profile = manifest
        .get("profile")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let session_id = manifest
        .get("sessionId")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let duration_ms = manifest
        .get("durationMs")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let ok = manifest.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);

    // Trajectory-derived metrics (best effort). The prompt provider doesn't
    // write a trajectory file — its runs are structurally single-turn, so we
    // default to turns=1 when the dispatch succeeded. coding-task and any
    // future trajectory-aware provider gets the real count from the jsonl.
    let traj_path = run_dir.join("trajectory.jsonl");
    let mut turns = if !traj_path.exists() && provider == "prompt" && ok {
        1u32
    } else {
        0u32
    };
    let mut compactions = 0u32;
    let mut tokens_before: Vec<u64> = Vec::new();
    if traj_path.exists() {
        let mut seen_summaries = std::collections::HashSet::new();
        for line in fs::read_to_string(&traj_path)?.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(ev) = serde_json::from_str::<Value>(line) else { continue };
            if ev.get("type").and_then(|t| t.as_str()) != Some("prompt.submitted") {
                continue;
            }
            turns += 1;
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
                            compactions += 1;
                            tokens_before.push(
                                m.get("tokensBefore")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0),
                            );
                        }
                    }
                }
            }
        }
    }

    let mut summary = String::new();
    summary.push_str(&format!(
        "- run: {session_id}\n- workload: {workload}\n- provider: {provider}\n- profile: {profile}\n- ok: {ok}\n- wall: {}s\n- turns: {turns}\n- compactions: {compactions}\n",
        duration_ms / 1000
    ));
    if !tokens_before.is_empty() {
        let listed: Vec<String> = tokens_before.iter().map(|n| n.to_string()).collect();
        summary.push_str(&format!("- tokensBefore: {}\n", listed.join(", ")));
    }

    // Try to include the agent's reply preview.
    let reply_path = run_dir.join("qa-reply.json");
    if reply_path.exists() {
        if let Ok(raw) = fs::read_to_string(&reply_path) {
            let preview = extract_reply_text(&raw);
            if !preview.is_empty() {
                let truncated: String = preview.chars().take(800).collect();
                summary.push_str(&format!("\n## reply preview (first 800 chars)\n\n{truncated}\n"));
            }
        }
    }

    Ok(summary)
}

fn extract_reply_text(stdout: &str) -> String {
    if stdout.trim().is_empty() {
        return String::new();
    }
    let Ok(parsed) = serde_json::from_str::<Value>(stdout) else {
        return stdout.to_string();
    };
    if let Some(payloads) = parsed
        .get("result")
        .and_then(|r| r.get("payloads"))
        .and_then(|p| p.as_array())
    {
        return payloads
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()).map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    if let Some(reply) = parsed.get("reply").and_then(|v| v.as_str()) {
        return reply.to_string();
    }
    String::new()
}

fn slugify(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '-',
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn shortid(run_id: &str) -> String {
    run_id.chars().take(12).collect()
}

fn chrono_like_today() -> String {
    use std::time::Duration;
    // Avoid pulling in chrono just for this; format YYYY-MM-DD from the
    // system clock using SystemTime.
    let now = SystemTime::now();
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs() as i64;
    let (y, m, d) = epoch_secs_to_ymd(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert unix epoch seconds to (year, month, day) in UTC. Civil calendar
/// algorithm from Howard Hinnant ("date-time"), public-domain.
fn epoch_secs_to_ymd(secs: i64) -> (i32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn slugify_lowercases_and_collapses() {
        assert_eq!(slugify("Hello World!"), "hello-world");
        assert_eq!(slugify("ABC--def"), "abc-def");
        assert_eq!(slugify("---"), "");
        assert_eq!(slugify("quick-q"), "quick-q");
    }

    #[test]
    fn shortid_truncates_to_12() {
        assert_eq!(shortid("abcdefghijklmnopqrstuvwxyz"), "abcdefghijkl");
        assert_eq!(shortid("short"), "short");
    }

    #[test]
    fn epoch_to_ymd_known_dates() {
        // Unix epoch start
        assert_eq!(epoch_secs_to_ymd(0), (1970, 1, 1));
        // 2000-02-29 leap day
        assert_eq!(epoch_secs_to_ymd(951_782_400), (2000, 2, 29));
        // 2024-02-29 leap day
        assert_eq!(epoch_secs_to_ymd(1_709_164_800), (2024, 2, 29));
        // 2024-03-01 (non-leap day after leap)
        assert_eq!(epoch_secs_to_ymd(1_709_251_200), (2024, 3, 1));
        // Round-trip a fresh epoch — value should be in current decade
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let (y, m, d) = epoch_secs_to_ymd(now);
        assert!((2024..=2030).contains(&y), "year {y} not in expected range");
        assert!((1..=12).contains(&m));
        assert!((1..=31).contains(&d));
    }

    #[test]
    fn extract_reply_payloads_array() {
        let json = r#"{"result":{"payloads":[{"text":"alpha"},{"text":"beta"}]}}"#;
        assert_eq!(extract_reply_text(json), "alpha\n\nbeta");
    }

    #[test]
    fn extract_reply_top_level() {
        let json = r#"{"reply":"hi"}"#;
        assert_eq!(extract_reply_text(json), "hi");
    }

    #[test]
    fn extract_reply_unknown() {
        assert_eq!(extract_reply_text("not json"), "not json");
        assert_eq!(extract_reply_text(""), "");
    }

    #[test]
    fn build_run_data_summary_works() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path();
        let manifest_json = serde_json::json!({
            "schemaVersion": 1,
            "workload": "quick-q",
            "provider": "prompt",
            "profile": "scribe",
            "sessionId": "abc-123",
            "durationMs": 12_000,
            "ok": true
        });
        let summary = build_run_data_summary(run_dir, &manifest_json).unwrap();
        assert!(summary.contains("workload: quick-q"));
        assert!(summary.contains("profile: scribe"));
        assert!(summary.contains("wall: 12s"));
        assert!(summary.contains("ok: true"));
    }

    /// Regression: a successful prompt-provider run with no trajectory file
    /// must report `turns: 1` (not 0). The prompt provider is structurally
    /// single-turn — a 0 here would have the agent draft an entry claiming
    /// the dispatch never happened.
    #[test]
    fn build_run_data_summary_prompt_provider_reports_one_turn() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path();
        let manifest_json = serde_json::json!({
            "workload": "quick-q",
            "provider": "prompt",
            "profile": "deep",
            "sessionId": "s",
            "durationMs": 8000,
            "ok": true
        });
        let summary = build_run_data_summary(run_dir, &manifest_json).unwrap();
        assert!(
            summary.contains("turns: 1"),
            "expected turns: 1 in summary, got: {summary}"
        );
        assert!(summary.contains("compactions: 0"));
    }

    /// A *failed* prompt-provider run (ok=false) still reports turns=0,
    /// since the dispatch may have died before reaching the model.
    #[test]
    fn build_run_data_summary_prompt_provider_failed_reports_zero_turns() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path();
        let manifest_json = serde_json::json!({
            "workload": "quick-q",
            "provider": "prompt",
            "profile": "deep",
            "sessionId": "s",
            "durationMs": 1000,
            "ok": false
        });
        let summary = build_run_data_summary(run_dir, &manifest_json).unwrap();
        assert!(summary.contains("turns: 0"));
    }

    #[test]
    fn build_run_data_summary_with_trajectory() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path();
        let trajectory = r#"{"type":"prompt.submitted","data":{"messages":[]}}
{"type":"prompt.submitted","data":{"messages":[{"role":"compactionSummary","summary":"alpha summary","tokensBefore":48000}]}}
"#;
        fs::write(run_dir.join("trajectory.jsonl"), trajectory).unwrap();
        let manifest_json = serde_json::json!({
            "workload": "long-task",
            "provider": "coding-task",
            "profile": "deep",
            "sessionId": "x",
            "durationMs": 200000,
            "ok": true
        });
        let summary = build_run_data_summary(run_dir, &manifest_json).unwrap();
        assert!(summary.contains("turns: 2"));
        assert!(summary.contains("compactions: 1"));
        assert!(summary.contains("tokensBefore: 48000"));
    }

    #[test]
    fn draft_entry_dry_run_does_not_dispatch() {
        let tmp = TempDir::new().unwrap();
        let darkmux = tmp.path().join(".darkmux");
        let runs_dir = darkmux.join("runs/test-run");
        fs::create_dir_all(&runs_dir).unwrap();
        fs::write(
            runs_dir.join("manifest.json"),
            r#"{"workload":"quick-q","provider":"prompt","profile":"scribe","sessionId":"s","durationMs":5000,"ok":true}"#,
        )
        .unwrap();

        let prev = env::current_dir().unwrap();
        env::set_current_dir(tmp.path()).unwrap();
        let report = draft_entry(&DraftOptions {
            run_id: "test-run".to_string(),
            agent: "main".to_string(),
            slug: Some("test-slug".to_string()),
            dry_run: true,
        })
        .unwrap();
        env::set_current_dir(prev).unwrap();

        assert!(report.run_dir.ends_with("test-run"));
        assert!(report.entry_path.to_str().unwrap().contains("test-slug"));
        assert!(report.prompt_chars > 100);
        // Dry run: file not written.
        assert!(!report.entry_path.exists());
    }

    #[test]
    fn draft_entry_errors_on_missing_run() {
        let tmp = TempDir::new().unwrap();
        let prev = env::current_dir().unwrap();
        env::set_current_dir(tmp.path()).unwrap();
        let result = draft_entry(&DraftOptions {
            run_id: "nonexistent".to_string(),
            agent: "main".to_string(),
            slug: None,
            dry_run: true,
        });
        env::set_current_dir(prev).unwrap();
        assert!(result.is_err());
    }
}
