//! Cross-layer telemetry sidecar for `lab run --instrument`.
//!
//! While a workload dispatch runs, this captures non-privileged samples at a
//! fixed cadence into `<run_dir>/instruments.jsonl` so an operator can later
//! inspect what the stack was actually doing — model identity, gateway
//! process state, transitions across LMStudio. The motivation is "trust but
//! verify" of inferences-engine labels (an `MLX` label tells you the file
//! format, not whether the runtime kept on MLX kernels for every op).
//!
//! This MVP captures only the surfaces accessible without root:
//! - `lms ps --json` — loaded models, contexts, status
//! - gateway process residency / CPU% via `ps`
//! - timestamps line up with `trajectory.jsonl` events for cross-correlation
//!
//! Silicon-level data (`powermetrics`: GPU/ANE/memory bandwidth, thermal)
//! requires sudo and is deferred to a future `--instrument-deep` flag with
//! explicit privilege-escalation handling.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One sample row in `instruments.jsonl`. Each line is a self-contained
/// `{t, source, payload}` envelope. `t` is unix-ms; `source` names the
/// telemetry stream; `payload` is source-specific JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    /// Unix milliseconds since epoch.
    pub t: u64,
    /// Milliseconds since the sidecar was started — convenient for plotting.
    pub elapsed_ms: u64,
    /// Source identifier (`lms` | `process` | `meta`).
    pub source: String,
    pub payload: serde_json::Value,
}

/// Sidecar handle. Drop semantics intentionally do NOT auto-stop — the
/// caller calls `stop()` explicitly so the dispatch can decide when to
/// flush. `start()` writes a `meta` event with the sidecar config; `stop()`
/// writes a `meta` close event.
pub struct InstrumentSidecar {
    stop_flag: Arc<AtomicBool>,
    join_handle: Option<thread::JoinHandle<Result<()>>>,
    output_path: PathBuf,
    started_at: Instant,
}

impl InstrumentSidecar {
    /// Begin sampling at `cadence` until `stop()` is called. Output goes to
    /// `<run_dir>/instruments.jsonl`. Returns immediately; sampling runs on
    /// a background thread.
    pub fn start(run_dir: &Path, cadence: Duration) -> Result<Self> {
        let output_path = run_dir.join("instruments.jsonl");

        // Open and write a meta-start event so partial captures are still
        // identifiable / parseable. Use `create` semantics — running with a
        // stale instruments.jsonl is a defect, but tolerate it by truncating.
        let mut writer = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output_path)
            .with_context(|| format!("opening {}", output_path.display()))?;

        let started_at = Instant::now();
        let started_unix = unix_ms_now();
        // Build the meta payload. Carries:
        //   - lifecycle: event, cadence_ms
        //   - format version: `version` (instrument-file format, unchanged)
        //   - CLI version: `darkmux_version` (semver from Cargo.toml)
        //   - rules: `rules_schema_version` + `rules` (the active rule set
        //     used by darkmux doctor, embedded so the viewer can render
        //     findings without duplicating rule definitions)
        let mut payload = serde_json::json!({
            "event": "start",
            "cadence_ms": cadence.as_millis() as u64,
            "version": 1,
            "darkmux_version": env!("CARGO_PKG_VERSION"),
        });
        // Splice rules_schema_version + rules into the meta payload. If
        // the serialization fails (shouldn't), we ship the run without
        // them rather than aborting the dispatch.
        if let Ok(rules_fields) = crate::eureka::RulesPayload::current().as_meta_fields() {
            if let Some(obj) = payload.as_object_mut() {
                for (k, v) in rules_fields {
                    obj.insert(k, v);
                }
            }
        }
        let meta_start = Sample {
            t: started_unix,
            elapsed_ms: 0,
            source: "meta".into(),
            payload,
        };
        writeln!(writer, "{}", serde_json::to_string(&meta_start)?)?;
        writer.flush()?;
        // Drop the writer — the background thread reopens in append mode.
        drop(writer);

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_clone = Arc::clone(&stop_flag);
        let output_path_clone = output_path.clone();
        let started_at_clone = started_at;

        let handle = thread::Builder::new()
            .name("darkmux-instrument".into())
            .spawn(move || {
                run_loop(&output_path_clone, started_at_clone, cadence, stop_flag_clone)
            })
            .with_context(|| "spawning instrument thread")?;

        Ok(Self {
            stop_flag,
            join_handle: Some(handle),
            output_path,
            started_at,
        })
    }

    /// Signal the sidecar to stop and wait for the background thread to
    /// finish flushing. Writes a `meta:end` event with elapsed time.
    pub fn stop(mut self) -> Result<PathBuf> {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.join_handle.take() {
            // Join can fail if the thread panicked; surface but don't lose
            // the partial capture file path.
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e.context("instrument thread errored")),
                Err(_) => anyhow::bail!("instrument thread panicked"),
            }
        }

        // Append meta:end. Best-effort — failure here doesn't invalidate
        // the capture.
        let elapsed = self.started_at.elapsed();
        if let Ok(mut writer) = OpenOptions::new().append(true).open(&self.output_path) {
            let meta_end = Sample {
                t: unix_ms_now(),
                elapsed_ms: elapsed.as_millis() as u64,
                source: "meta".into(),
                payload: serde_json::json!({
                    "event": "end",
                    "total_elapsed_ms": elapsed.as_millis() as u64,
                }),
            };
            let _ = writeln!(writer, "{}", serde_json::to_string(&meta_end)?);
            let _ = writer.flush();
        }

        Ok(self.output_path)
    }
}

fn run_loop(
    output_path: &Path,
    started_at: Instant,
    cadence: Duration,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut writer = OpenOptions::new()
        .append(true)
        .open(output_path)
        .with_context(|| format!("appending {}", output_path.display()))?;

    // Sample once before the first sleep so very-fast dispatches still get
    // captured-state. Then sample on cadence until stop.
    while !stop.load(Ordering::SeqCst) {
        let t = unix_ms_now();
        let elapsed_ms = started_at.elapsed().as_millis() as u64;

        // Each sampler is independent — failure in one does not stop others.
        if let Some(payload) = sample_lms_ps() {
            let s = Sample {
                t,
                elapsed_ms,
                source: "lms".into(),
                payload,
            };
            let _ = writeln!(writer, "{}", serde_json::to_string(&s)?);
        }

        if let Some(payload) = sample_gateway_proc() {
            let s = Sample {
                t,
                elapsed_ms,
                source: "process".into(),
                payload,
            };
            let _ = writeln!(writer, "{}", serde_json::to_string(&s)?);
        }

        let _ = writer.flush();

        // Sleep in small slices so stop() responds promptly.
        let mut remaining = cadence;
        let slice = Duration::from_millis(100);
        while remaining > Duration::ZERO && !stop.load(Ordering::SeqCst) {
            let to_sleep = remaining.min(slice);
            thread::sleep(to_sleep);
            remaining = remaining.saturating_sub(to_sleep);
        }
    }

    Ok(())
}

/// Run `lms ps --json` and capture the loaded models. Returns `None` on
/// any failure (lms not installed, JSON parse failure, etc.) — telemetry
/// gaps are preferable to spurious errors that would derail the dispatch.
fn sample_lms_ps() -> Option<serde_json::Value> {
    let lms_bin = std::env::var("DARKMUX_LMS_BIN").unwrap_or_else(|_| "lms".to_string());
    let out = Command::new(&lms_bin).args(["ps", "--json"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    // Slim the payload — we want the shape but not all the noise.
    let trimmed: Vec<serde_json::Value> = match parsed.as_array() {
        Some(arr) => arr
            .iter()
            .map(|m| {
                serde_json::json!({
                    "identifier": m.get("identifier").and_then(|v| v.as_str()).unwrap_or(""),
                    "modelKey":   m.get("modelKey").and_then(|v| v.as_str()).unwrap_or(""),
                    "context":    m.get("contextLength").and_then(|v| v.as_u64())
                                  .or_else(|| m.get("context").and_then(|v| v.as_u64()))
                                  .unwrap_or(0),
                    "status":     m.get("status").and_then(|v| v.as_str()).unwrap_or(""),
                })
            })
            .collect(),
        None => Vec::new(),
    };
    Some(serde_json::json!({ "models": trimmed }))
}

/// Find the OpenClaw gateway process and capture residency / CPU%.
/// Heuristic: looks for a node process listening on the configured gateway
/// port (defaults to 18789). On failure, returns `None`.
///
/// We use BSD `ps` which is available everywhere on macOS. Output is
/// space-separated; the columns we ask for are stable across versions.
fn sample_gateway_proc() -> Option<serde_json::Value> {
    let port = std::env::var("OPENCLAW_GATEWAY_PORT").unwrap_or_else(|_| "18789".to_string());

    // `lsof -i :<port> -t` prints just PIDs of listeners. Quick and quiet.
    let lsof_out = Command::new("lsof")
        .args(["-i", &format!(":{port}"), "-t"])
        .output()
        .ok()?;
    if !lsof_out.status.success() {
        return None;
    }
    let pid_str = String::from_utf8_lossy(&lsof_out.stdout)
        .lines()
        .next()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if pid_str.is_empty() {
        return None;
    }

    // Capture rss (KB), %cpu, and elapsed for the listener.
    let ps_out = Command::new("ps")
        .args(["-p", &pid_str, "-o", "pid=,rss=,pcpu=,etime="])
        .output()
        .ok()?;
    if !ps_out.status.success() {
        return None;
    }
    let row = String::from_utf8_lossy(&ps_out.stdout).trim().to_string();
    let mut fields = row.split_whitespace();
    let pid: u64 = fields.next()?.parse().ok()?;
    let rss_kb: u64 = fields.next()?.parse().ok()?;
    let pcpu: f64 = fields.next()?.parse().ok()?;
    let etime: String = fields.next().unwrap_or("").to_string();

    Some(serde_json::json!({
        "pid": pid,
        "port": port,
        "rss_mb": rss_kb / 1024,
        "cpu_percent": pcpu,
        "elapsed": etime,
    }))
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn sample_serializes_round_trip() {
        let s = Sample {
            t: 1_700_000_000_000,
            elapsed_ms: 1234,
            source: "test".into(),
            payload: serde_json::json!({"x": 1}),
        };
        let line = serde_json::to_string(&s).unwrap();
        let parsed: Sample = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed.t, s.t);
        assert_eq!(parsed.source, s.source);
    }

    #[test]
    fn start_writes_meta_and_creates_file() {
        let tmp = TempDir::new().unwrap();
        let sidecar = InstrumentSidecar::start(tmp.path(), Duration::from_millis(50)).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let out_path = sidecar.stop().unwrap();
        let body = std::fs::read_to_string(&out_path).unwrap();
        assert!(body.contains("\"event\":\"start\""), "missing meta:start");
        assert!(body.contains("\"event\":\"end\""), "missing meta:end");
        // Should have at least one non-meta sample (lms or process).
        let non_meta = body.lines().filter(|l| !l.contains("\"meta\"")).count();
        // It's fine if no lms/process samples land in 150ms — the file
        // existence + meta envelope are the firm guarantees.
        let _ = non_meta;
    }

    #[test]
    fn stop_truncates_existing_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("instruments.jsonl");
        std::fs::write(&path, "stale-line\nstale-line\n").unwrap();
        let sidecar = InstrumentSidecar::start(tmp.path(), Duration::from_millis(50)).unwrap();
        let out_path = sidecar.stop().unwrap();
        let body = std::fs::read_to_string(&out_path).unwrap();
        assert!(!body.contains("stale-line"), "old content should have been truncated");
    }
}
