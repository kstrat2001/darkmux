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
                summary_chars: summary_text.len(),
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

    let paths = paths::resolve(ResolveScope::Auto);
    let loaded = load(workload_id, Some(&paths.root))?;

    let report = with_provider(provider_id, |p| p.inspect(&loaded, &run_dir))??;
    Ok(report)
}

/// Summary of cross-layer telemetry captured by `--instrument` during a
/// run. Only present if `instruments.jsonl` exists in the run dir.
#[derive(Debug, Clone)]
pub struct TelemetrySummary {
    /// Total wall-clock duration the sidecar was active, in seconds.
    pub elapsed_s: u64,
    /// How many lms-state samples landed.
    pub lms_samples: usize,
    /// How many process-state samples landed.
    pub process_samples: usize,
    /// Distinct LMStudio identifiers observed across the run (e.g. did a
    /// model change mid-dispatch?).
    pub model_identifiers_seen: Vec<String>,
    /// Peak gateway-process resident memory observed, in MB.
    pub gateway_peak_rss_mb: u64,
    /// Mean gateway-process CPU% observed during the dispatch.
    pub gateway_mean_cpu: f64,
    /// Anomaly findings the inspector should surface to the operator.
    pub anomalies: Vec<String>,
}

/// Read `instruments.jsonl` from a run dir and compute the summary above.
/// Returns `Ok(None)` if no telemetry was captured (no `--instrument` on
/// the original run, or the sidecar failed to start). Returns `Err` only
/// for genuinely malformed files — any individual unparseable line is
/// skipped silently because the format is best-effort append-only.
pub fn read_telemetry_summary(run_dir: &Path) -> Result<Option<TelemetrySummary>> {
    let path = run_dir.join("instruments.jsonl");
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;

    let mut elapsed_s = 0u64;
    let mut lms_samples = 0usize;
    let mut process_samples = 0usize;
    let mut model_identifiers: std::collections::BTreeSet<String> = Default::default();
    let mut peak_rss_mb = 0u64;
    let mut cpu_sum = 0.0f64;
    let mut cpu_count = 0u64;
    let mut sample_count_per_model_set: std::collections::BTreeMap<String, usize> = Default::default();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else { continue };
        let source = ev.get("source").and_then(|v| v.as_str()).unwrap_or("");
        match source {
            "meta"
                if ev
                    .get("payload")
                    .and_then(|p| p.get("event"))
                    .and_then(|v| v.as_str())
                    == Some("end") =>
            {
                elapsed_s = ev
                    .get("payload")
                    .and_then(|p| p.get("total_elapsed_ms"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    / 1000;
            }
            "lms" => {
                lms_samples += 1;
                if let Some(models) =
                    ev.get("payload").and_then(|p| p.get("models")).and_then(|m| m.as_array())
                {
                    let mut current_set: Vec<String> = models
                        .iter()
                        .filter_map(|m| {
                            m.get("identifier").and_then(|v| v.as_str()).map(|s| s.to_string())
                        })
                        .collect();
                    current_set.sort();
                    for id in &current_set {
                        if !id.is_empty() {
                            model_identifiers.insert(id.clone());
                        }
                    }
                    let key = current_set.join(",");
                    *sample_count_per_model_set.entry(key).or_insert(0) += 1;
                }
            }
            "process" => {
                process_samples += 1;
                let payload = ev.get("payload");
                if let Some(rss) = payload.and_then(|p| p.get("rss_mb")).and_then(|v| v.as_u64()) {
                    if rss > peak_rss_mb {
                        peak_rss_mb = rss;
                    }
                }
                if let Some(cpu) = payload.and_then(|p| p.get("cpu_percent")).and_then(|v| v.as_f64()) {
                    cpu_sum += cpu;
                    cpu_count += 1;
                }
            }
            _ => {}
        }
    }

    let mean_cpu = if cpu_count > 0 { cpu_sum / cpu_count as f64 } else { 0.0 };

    // Anomaly rules — keep simple and noisy-on-the-side-of-helpful for MVP.
    let mut anomalies: Vec<String> = Vec::new();
    if sample_count_per_model_set.len() > 1 {
        anomalies.push(format!(
            "loaded-model set changed during the run ({} distinct sets observed) — possible JIT load or external swap",
            sample_count_per_model_set.len()
        ));
    }
    if lms_samples == 0 && process_samples > 0 {
        anomalies.push("no LMStudio state captured — `lms` CLI may not be installed or queryable".into());
    }
    if process_samples == 0 && lms_samples > 0 {
        anomalies.push("no gateway-process state captured — port-listener detection failed".into());
    }
    // Note: a previous version of this rule flagged "mean gateway CPU < 1%
    // = dispatch went to a different process." That's wrong-shaped. The
    // gateway is a thin orchestrator; the heavy CPU+GPU work happens in
    // LMStudio (a separate process) and isn't visible in `ps -p
    // <gateway-pid>`. Real long-agentic dispatches routinely show 0.1-0.5%
    // mean gateway CPU. The right "is dispatch active?" signal is the
    // lms.payload.models[*].status field ("generating" vs "idle"), not
    // gateway-process CPU. Rule dropped.

    Ok(Some(TelemetrySummary {
        elapsed_s,
        lms_samples,
        process_samples,
        model_identifiers_seen: model_identifiers.into_iter().collect(),
        gateway_peak_rss_mb: peak_rss_mb,
        gateway_mean_cpu: mean_cpu,
        anomalies,
    }))
}

fn resolve_run_dir(path: &str) -> PathBuf {
    if path.starts_with('/') || path.starts_with("./") || path.starts_with("../") || path.contains('/') {
        return PathBuf::from(path);
    }
    let paths = paths::resolve(ResolveScope::Auto);
    let candidate = paths.runs.join(path);
    if candidate.exists() {
        return candidate;
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
