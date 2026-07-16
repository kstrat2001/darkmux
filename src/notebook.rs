//! `darkmux notebook draft <run-id>` — role-as-scribe for lab notebook entries.
//!
//! Reads a lab run's manifest + trajectory, builds a prompt asking the active
//! runtime to draft a lab-notebook entry, dispatches it (typically against the
//! `scribe` role), and writes the draft to .darkmux/notebook/<date>-<slug>.md.
//!
//! Routes through the internal Docker-bounded runtime — the only dispatch
//! path (#1405 removed the legacy openclaw shell-out runtime).

use crate::crew::dispatch::{DispatchOpts, Runtime};
use crate::lab::paths::{self, ResolveScope};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct DraftOptions {
    pub run_id: String,
    /// DM role id to dispatch the drafting prompt through. The role
    /// resolves through `templates/builtin/roles/<role>.{json,md}`
    /// under the internal runtime.
    pub role: String,
    pub slug: Option<String>,
    pub dry_run: bool,
    /// Override the machine id (overrides DARKMUX_MACHINE_ID env var).
    pub machine_override: Option<String>,
    /// Which agent runtime to dispatch the drafting prompt through.
    /// `Runtime::Internal` is the only value.
    pub runtime: Runtime,
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

    // Dispatch through darkmux's in-house Docker-bounded runtime via
    // `crew::dispatch::dispatch()`, mirroring the pattern used by
    // `lab run` + `dispatch`.
    let session_id = format!(
        "darkmux-notebook-{}-{}",
        opts.run_id,
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );

    let entry_text = if opts.dry_run {
        let Runtime::Internal = opts.runtime;
        format!(
            "[DRY RUN — would have dispatched the prompt below to internal runtime via role `{}`]\n\n{prompt}",
            opts.role
        )
    } else {
        let Runtime::Internal = opts.runtime;
        dispatch_draft_via_internal(&opts.role, &prompt, &session_id)?
    };

    // Compose the entry path: <date>-<slug>.md
    let date = darkmux_flow::day_utc_now();
    let slug = opts.slug.clone().unwrap_or_else(|| {
        slugify(&format!(
            "{}-{}",
            manifest
                .get("workload")
                .and_then(|v| v.as_str())
                .unwrap_or("run"),
            shortid(&opts.run_id)
        ))
    });
    // Notebook dir resolves env > config.dirs.notebook > <root>/notebook (#661
    // Slice 3) — config can point it at an iCloud-synced path, like the env var.
    let notebook_dir = darkmux_types::config_access::notebook_dir();
    let entry_path = notebook_dir.join(format!("{date}-{slug}.md"));

    if !opts.dry_run {
        if !notebook_dir.exists() {
            fs::create_dir_all(&notebook_dir)?;
        }
        // Include a machine-id tag so the same notebook directory (e.g.
        // when pointed at an iCloud-synced path via DARKMUX_NOTEBOOK_DIR)
        // can hold entries from multiple machines without ambiguity.
        let machine_id = resolve_machine_id(opts);
        let header = format!(
            "<!-- darkmux:notebook-entry: run={} machine={} date={} -->\n\n",
            opts.run_id, machine_id, date
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

/// Dispatch the drafting prompt through darkmux's internal Docker-bounded
/// runtime via `crew::dispatch::dispatch()`. Reads the role manifest from
/// `templates/builtin/roles/<role>.{json,md}` (or the operator's override
/// under `~/.darkmux/roles/`).
fn dispatch_draft_via_internal(role: &str, prompt: &str, session_id: &str) -> Result<String> {
    let opts = DispatchOpts {
        role_id: role.to_string(),
        message: prompt.to_string(),
        deliver: None,
        session_id: Some(session_id.to_string()),
        timeout_seconds: 1800,
        skip_preflight: false,
        // notebook draft parses reply text from the JSON envelope's
        // `.final_assistant` field (same shape Phase-B established
        // for qa-review).
        json: true,
        workdir: None,
        phase_id: None,
        runtime: Runtime::Internal,
        machine: None,
        wait: true,
        // Notebook draft is single-shot prompt-style; compaction
        // defaults are fine.
        compaction: crate::crew::dispatch::CompactionDispatchArgs::default(),
        // (#549) No `--profile` override; fall back to `default_profile`.
        profile_name: None,
        // (#984) No --profiles-file here; dispatch resolves from env > default.
        config_path: None,
        // (#703) default image.
        // (#1199) Bench-only knobs; defaults preserve existing behavior.
        force_container: false,
        max_completion_tokens: None,
        image: None,
        model_base_url_override: None,
    };
    let result = crate::fleet::dispatch_routed(opts)
        .context("internal-runtime dispatch for notebook draft")?;
    if result.exit_code != 0 {
        bail!(
            "internal-runtime notebook draft failed (exit {}): {}",
            result.exit_code,
            result.stderr.trim()
        );
    }
    Ok(extract_reply_text(&result.stdout))
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
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let duration_ms = manifest
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let ok = manifest
        .get("ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

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
            let Ok(ev) = serde_json::from_str::<Value>(line) else {
                continue;
            };
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
                            tokens_before
                                .push(m.get("tokensBefore").and_then(|v| v.as_u64()).unwrap_or(0));
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
                summary.push_str(&format!(
                    "\n## reply preview (first 800 chars)\n\n{truncated}\n"
                ));
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
    // Try the DM internal-runtime JSON envelope first (Phase-A shape:
    // `{result, final_assistant, metrics, trajectory_path}`). Falls
    // through to legacy `payloads[]` / `reply` shapes for reading
    // historical run artifacts predating the internal runtime.
    if let Some(final_assistant) = parsed.get("final_assistant").and_then(|v| v.as_str()) {
        return final_assistant.to_string();
    }
    let payloads = parsed
        .get("payloads")
        .or_else(|| parsed.get("result").and_then(|r| r.get("payloads")))
        .and_then(|p| p.as_array());
    if let Some(payloads) = payloads {
        return payloads
            .iter()
            .filter_map(|p| {
                p.get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            })
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

/// Resolve the machine id for a notebook entry. Priority:
///   1. Explicit `--machine` flag (i.e. `DraftOptions::machine_override`)
///   2. `DARKMUX_MACHINE_ID` env var
///   3. Auto-derived hardware fingerprint
///
/// Extracted as a helper so the priority chain is testable without
/// requiring Docker to be running (the dispatch path in `draft_entry`
/// would otherwise prevent CI coverage on machines without it).
pub(crate) fn resolve_machine_id(opts: &DraftOptions) -> String {
    opts.machine_override
        .clone()
        .unwrap_or_else(machine_fingerprint)
}

/// Identifier tag for the machine that drafted this notebook entry.
///
/// Priority:
///   1. `DARKMUX_MACHINE_ID` env var (operator-named, e.g. `m5-max-home`)
///   2. Auto-derived fingerprint: `<platform-slug>-<ram-gb>gb`
///   3. Hard fallback: `"unknown"` if hardware detection somehow fails
///
/// Operators sharing a notebook directory across machines (typically via
/// DARKMUX_NOTEBOOK_DIR pointing at an iCloud-synced path) should set
/// DARKMUX_MACHINE_ID on each host so entries are unambiguously
/// attributable in cross-machine readouts.
fn machine_fingerprint() -> String {
    if let Ok(explicit) = env::var("DARKMUX_MACHINE_ID") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let spec = crate::hardware::detect();
    let platform_slug = match spec.platform {
        crate::hardware::Platform::AppleSilicon => "apple-silicon",
        crate::hardware::Platform::MacIntel => "mac-intel",
        crate::hardware::Platform::Linux => "linux",
        crate::hardware::Platform::Windows => "windows",
        crate::hardware::Platform::Other => "unknown-platform",
    };
    if spec.total_ram_gb == 0 {
        platform_slug.to_string()
    } else {
        format!("{platform_slug}-{}gb", spec.total_ram_gb)
    }
}

/// A single notebook entry parsed from file-header metadata.
pub struct NotebookEntry {
    pub date: String,
    pub machine: String,
    pub run: String,
    pub path: PathBuf,
}

/// Enumerate notebook entries from a directory.
///
/// Reads each `.md` file in `dir`, parses the darkmux header comment
/// to extract run/machine/date metadata, and returns a list of entries.
/// Optionally filters by machine id (when `machine_filter` is Some).
pub fn list_entries(dir: &Path, machine_filter: Option<&str>) -> Result<Vec<NotebookEntry>> {
    let mut entries = Vec::new();
    let read_dir = dir
        .read_dir()
        .with_context(|| format!("reading notebook directory: {}", dir.display()))?;

    for entry in read_dir {
        let file = entry?.file_name();
        // Only process .md files.
        if file.to_string_lossy().ends_with(".md") {
            let path = dir.join(&file);
            if let Ok(header) = std::fs::read_to_string(&path) {
                // Parse the darkmux header comment.
                if let Some(notebook_entry) = parse_notebook_header(&header, &path) {
                    // Apply machine filter if specified.
                    match machine_filter {
                        Some(filter) if notebook_entry.machine != filter => continue,
                        _ => {}
                    }
                    entries.push(notebook_entry);
                }
            }
        }
    }

    // Sort by date descending (newest first).
    entries.sort_by(|a, b| b.date.cmp(&a.date).then(b.run.cmp(&a.run)));

    Ok(entries)
}

/// Parse a darkmux notebook-entry header from file content.
pub fn parse_notebook_header(content: &str, path: &Path) -> Option<NotebookEntry> {
    // Header is expected on line 1: `<!-- darkmux:notebook-entry: run=X machine=Y date=Z -->`
    let line = content.lines().next()?;
    // Strip HTML comment delimiters.
    let inner = line
        .strip_prefix("<!--")
        .and_then(|s| s.strip_suffix("-->"))?;
    // Parse key=value pairs.
    let mut run = String::new();
    let mut machine = String::new();
    let mut date = String::new();
    for pair in inner.split_whitespace() {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "run" => run = value.to_string(),
                "machine" => machine = value.to_string(),
                "date" => date = value.to_string(),
                _ => {}
            }
        }
    }

    if run.is_empty() || machine.is_empty() || date.is_empty() {
        return None;
    }

    Some(NotebookEntry {
        run,
        machine,
        date,
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[serial_test::serial]
    #[test]
    fn machine_fingerprint_honors_explicit_env_var() {
        let prev = env::var("DARKMUX_MACHINE_ID").ok();
        // Safety: tests in this module are #[serial_test::serial] so env mutation
        // is sequenced across the file's tests.
        unsafe {
            env::set_var("DARKMUX_MACHINE_ID", "m5-max-home");
        }
        assert_eq!(machine_fingerprint(), "m5-max-home");

        // Empty value falls back to auto-detection (not propagated as empty).
        unsafe {
            env::set_var("DARKMUX_MACHINE_ID", "   ");
        }
        let auto = machine_fingerprint();
        assert_ne!(auto, "");
        assert_ne!(auto, "   ");

        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_MACHINE_ID", v),
                None => env::remove_var("DARKMUX_MACHINE_ID"),
            }
        }
    }

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
    fn extract_reply_payloads_array() {
        let json = r#"{"result":{"payloads":[{"text":"alpha"},{"text":"beta"}]}}"#;
        assert_eq!(extract_reply_text(json), "alpha\n\nbeta");
    }

    #[test]
    fn extract_reply_payloads_top_level() {
        let json = r#"{"payloads":[{"text":"alpha","mediaUrl":null},{"text":"beta"}],"meta":{}}"#;
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

    /// Phase-H: extract_reply_text understands DM's internal-runtime
    /// JSON envelope (`final_assistant`). Without this, notebook draft
    /// against `--runtime internal` would return empty for the reply.
    #[test]
    fn extract_reply_dm_internal_envelope() {
        let json = r#"{"result":"stop","final_assistant":"Notebook draft body here.","metrics":{"turns":1},"trajectory_path":"/x/y.jsonl"}"#;
        assert_eq!(
            extract_reply_text(json),
            "Notebook draft body here."
        );
    }

    /// Phase-H: when both shapes are present (shouldn't happen in
    /// practice, but be deterministic), `final_assistant` wins — that's
    /// the DM-side envelope, and notebook draft now defaults internal.
    #[test]
    fn extract_reply_prefers_final_assistant_over_payloads() {
        let json = r#"{
            "final_assistant":"DM envelope wins",
            "payloads":[{"text":"OC envelope"}]
        }"#;
        assert_eq!(extract_reply_text(json), "DM envelope wins");
    }

    #[test]
    fn build_run_data_summary_works() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path();
        let manifest_json = serde_json::json!({
            "schema_version": 1,
            "workload": "quick-q",
            "provider": "prompt",
            "profile": "scribe",
            "session_id": "abc-123",
            "duration_ms": 12_000,
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
            "session_id": "s",
            "duration_ms": 8000,
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
            "session_id": "s",
            "duration_ms": 1000,
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
            "session_id": "x",
            "duration_ms": 200000,
            "ok": true
        });
        let summary = build_run_data_summary(run_dir, &manifest_json).unwrap();
        assert!(summary.contains("turns: 2"));
        assert!(summary.contains("compactions: 1"));
        assert!(summary.contains("tokensBefore: 48000"));
    }

    // Process-global state mutated below (cwd + DARKMUX_NOTEBOOK_DIR);
    // `#[serial]` keeps this test from racing other env-touching tests in
    // the same binary, and the explicit save/restore guards against any
    // operator-set DARKMUX_NOTEBOOK_DIR (e.g. iCloud notebook on the dev
    // rig) leaking the entry path outside the tempdir.
    #[serial_test::serial]
    #[test]
    fn draft_entry_dry_run_does_not_dispatch() {
        let tmp = TempDir::new().unwrap();
        let darkmux = tmp.path().join(".darkmux");
        let runs_dir = darkmux.join("runs/test-run");
        fs::create_dir_all(&runs_dir).unwrap();
        fs::write(
            runs_dir.join("manifest.json"),
            r#"{"workload":"quick-q","provider":"prompt","profile":"scribe","session_id":"s","duration_ms":5000,"ok":true}"#,
        )
        .unwrap();

        let prev_cwd = env::current_dir().unwrap();
        let prev_notebook = env::var("DARKMUX_NOTEBOOK_DIR").ok();
        // set_var / remove_var are `unsafe` since the 2024 edition due to
        // thread-safety; the surrounding `#[serial]` keeps this single-
        // threaded across the test binary, so the contract holds.
        unsafe { env::remove_var("DARKMUX_NOTEBOOK_DIR") };
        env::set_current_dir(tmp.path()).unwrap();

        let report = draft_entry(&DraftOptions {
            run_id: "test-run".to_string(),
            role: "scribe".to_string(),
            slug: Some("test-slug".to_string()),
            dry_run: true,
            machine_override: None,
            runtime: Runtime::Internal,
        })
        .unwrap();

        env::set_current_dir(&prev_cwd).unwrap();
        unsafe {
            match prev_notebook {
                Some(v) => env::set_var("DARKMUX_NOTEBOOK_DIR", v),
                None => env::remove_var("DARKMUX_NOTEBOOK_DIR"),
            }
        }

        assert!(report.run_dir.ends_with("test-run"));
        assert!(report.entry_path.to_str().unwrap().contains("test-slug"));
        assert!(report.prompt_chars > 100);
        // Dry run: file not written. The `remove_var` above ensures the
        // entry would have landed in `<tmpdir>/.darkmux/notebook/` rather
        // than the operator's iCloud notebook, so this can't false-positive
        // off a same-slug entry that happens to live in the real notebook.
        assert!(!report.entry_path.exists());
    }

    // Sibling to the dry-run test above — same cwd mutation, same
    // serial requirement so the two don't race each other or any other
    // env-touching test.
    #[serial_test::serial]
    #[test]
    fn draft_entry_errors_on_missing_run() {
        let tmp = TempDir::new().unwrap();
        let prev = env::current_dir().unwrap();
        env::set_current_dir(tmp.path()).unwrap();
        let result = draft_entry(&DraftOptions {
            run_id: "nonexistent".to_string(),
            role: "scribe".to_string(),
            slug: None,
            dry_run: true,
            machine_override: None,
            runtime: Runtime::Internal,
        });
        env::set_current_dir(prev).unwrap();
        assert!(result.is_err());
    }

    /// list_entries returns entries from .md files and ignores non-.md.
    #[test]
    fn list_entries_parses_md_files() {
        let tmp = TempDir::new().unwrap();
        // Create two .md files with headers.
        fs::write(
            tmp.path().join("2026-05-10-run-a.md"),
            "<!-- darkmux:notebook-entry: run=abc123 machine=m5-home date=2026-05-10 -->\n\nSome content.",
        )
        .unwrap();
        fs::write(
            tmp.path().join("2026-05-11-run-b.md"),
            "<!-- darkmux:notebook-entry: run=def456 machine=m3-laptop date=2026-05-11 -->\n\nOther content.",
        )
        .unwrap();
        // A non-.md file should be ignored.
        fs::write(tmp.path().join("readme.txt"), "not a notebook entry").unwrap();

        let entries = list_entries(tmp.path(), None).unwrap();
        assert_eq!(entries.len(), 2);
        // Should be sorted newest first.
        assert_eq!(entries[0].date, "2026-05-11");
        assert_eq!(entries[1].date, "2026-05-10");
    }

    /// list_entries filters by machine when specified.
    #[test]
    fn list_entries_filters_by_machine() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("e1.md"),
            "<!-- darkmux:notebook-entry: run=r1 machine=m5-home date=2026-05-10 -->\n\nc",
        )
        .unwrap();
        fs::write(
            tmp.path().join("e2.md"),
            "<!-- darkmux:notebook-entry: run=r2 machine=m3-laptop date=2026-05-11 -->\n\nc",
        )
        .unwrap();

        // Filter to m5-home: only 1 entry.
        let filtered = list_entries(tmp.path(), Some("m5-home")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].machine, "m5-home");

        // Filter to m3-laptop: only 1 entry.
        let filtered2 = list_entries(tmp.path(), Some("m3-laptop")).unwrap();
        assert_eq!(filtered2.len(), 1);
        assert_eq!(filtered2[0].machine, "m3-laptop");

        // Filter to nonexistent machine: 0 entries.
        let filtered3 = list_entries(tmp.path(), Some("nonexistent")).unwrap();
        assert!(filtered3.is_empty());
    }

    /// parse_notebook_header correctly extracts run/machine/date.
    #[test]
    fn parse_header_extracts_all_fields() {
        let header =
            "<!-- darkmux:notebook-entry: run=xyz789 machine=m5-max-home date=2026-03-15 -->\n";
        let entry = parse_notebook_header(header, Path::new("test.md")).unwrap();
        assert_eq!(entry.run, "xyz789");
        assert_eq!(entry.machine, "m5-max-home");
        assert_eq!(entry.date, "2026-03-15");
    }

    /// parse_notebook_header returns None for malformed headers.
    #[test]
    fn parse_header_malformed_returns_none() {
        // Missing one field.
        let header =
            "<!-- darkmux:notebook-entry: run=abc machine=m5 date=2026-01-01 extra=val -->\n";
        let entry = parse_notebook_header(header, Path::new("test.md")).unwrap();
        assert_eq!(entry.run, "abc");
        assert_eq!(entry.machine, "m5");
        // Extra fields are ignored; entry still valid.

        // Completely broken header (no darkmux key).
        let bad = "<!-- not a darkmux comment -->\n";
        assert!(parse_notebook_header(bad, Path::new("test.md")).is_none());

        // Not a comment at all.
        assert!(parse_notebook_header("plain text", Path::new("test.md")).is_none());
    }

    /// resolve_machine_id honors --machine flag > DARKMUX_MACHINE_ID > fingerprint.
    /// Hits the helper directly so the test doesn't require Docker to be
    /// running (avoids CI failures on macos-latest runners).
    #[serial_test::serial]
    #[test]
    fn resolve_machine_id_priority_chain() {
        let prev = env::var("DARKMUX_MACHINE_ID").ok();

        // 1. Explicit override wins over both env var and fingerprint.
        unsafe {
            env::set_var("DARKMUX_MACHINE_ID", "env-fingerprint");
        }
        let opts_with_override = DraftOptions {
            run_id: "x".into(),
            role: "scribe".into(),
            slug: None,
            dry_run: true,
            machine_override: Some("override-machine".into()),
            runtime: Runtime::Internal,
        };
        assert_eq!(resolve_machine_id(&opts_with_override), "override-machine");

        // 2. With no override, env var wins over fingerprint.
        let opts_no_override = DraftOptions {
            run_id: "x".into(),
            role: "scribe".into(),
            slug: None,
            dry_run: true,
            machine_override: None,
            runtime: Runtime::Internal,
        };
        assert_eq!(resolve_machine_id(&opts_no_override), "env-fingerprint");

        // 3. With no override AND no env var, falls back to hardware fingerprint
        //    (non-empty per machine_fingerprint() contract).
        unsafe {
            env::remove_var("DARKMUX_MACHINE_ID");
        }
        let fp = resolve_machine_id(&opts_no_override);
        assert!(!fp.is_empty(), "fingerprint must not be empty");
        assert_ne!(fp, "override-machine");
        assert_ne!(fp, "env-fingerprint");

        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_MACHINE_ID", v),
                None => env::remove_var("DARKMUX_MACHINE_ID"),
            }
        }
    }
}
