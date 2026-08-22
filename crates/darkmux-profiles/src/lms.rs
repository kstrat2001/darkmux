use crate::gestalt_host::lms_host::{run_bounded, StdoutMode, DEFAULT_LIST_BOUND};
use crate::gestalt_host::resolved_load_deadline;
use anyhow::{Context, Result, bail};
use darkmux_gestalt::Deadline;
use darkmux_types::LoadedModel;
use std::process::Command;

// ─── bounded by construction (#1595) ────────────────────────────────────
//
// Every call in this file spawns the external `lms` CLI, and every call
// sits on a path an operator is actively WAITING on — the dispatch
// preflight (`ensure_model_loaded_at_ctx` calls `list_loaded` + `unload`
// before every local dispatch), the telemetry sampler, the swap paths. The
// `lms` CLI blocks on LMStudio's local API socket, so a wedged backend
// used to hang these calls — and with them the whole dispatch — forever,
// with no error and no diagnostic.
//
// That is the third instance of the unbounded-external-call class this
// project has paid for (#1570/#1573 removed it for Redis reads/writes,
// #1276 for the gestalt host port; the #1593 gate caught a fourth being
// born in `mission status`'s tailscale probe). The fix is the same shape
// every time, so these calls now route through the SAME bounded runner
// the gestalt adapter uses (`run_bounded`: spawn + poll + kill-at-deadline)
// instead of growing a bespoke fourth timeout:
//
//   - read-only lists  → `DEFAULT_LIST_BOUND` (30s, shared with `LmsHost`)
//   - unload / load    → `resolved_load_deadline()` — the operator-tunable
//                        `DARKMUX_MODEL_LOAD_TIMEOUT_SECONDS` (#1276)

// pub(crate): the gestalt host adapter (`gestalt_host::LmsHost`, #1274
// packet 2b) resolves its binary through the same single precedence home.
pub(crate) fn lms_bin() -> String {
    // env(DARKMUX_LMS_BIN) > config.lms_bin > "lms" (#661 Slice 4).
    darkmux_types::config_access::lms_bin()
}

pub fn list_loaded() -> Result<Vec<LoadedModel>> {
    let mut cmd = Command::new(lms_bin());
    cmd.args(["ps", "--json"]);
    let out = run_bounded(cmd, "ps", Deadline(DEFAULT_LIST_BOUND), StdoutMode::Capture)
        .map_err(|e| anyhow::anyhow!("running `lms ps --json`: {e}"))?;
    if out.status.success() {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&out.stdout) {
            if let Some(arr) = parsed.as_array() {
                return Ok(arr.iter().map(model_from_json).collect());
            }
        }
    }
    // fallback to text parsing — bounded the same way (a wedged `lms`
    // would hang the fallback just as readily as the primary).
    let mut cmd = Command::new(lms_bin());
    cmd.args(["ps"]);
    let text_out = run_bounded(cmd, "ps", Deadline(DEFAULT_LIST_BOUND), StdoutMode::Capture)
        .map_err(|e| anyhow::anyhow!("running `lms ps`: {e}"))?;
    Ok(parse_text_ps(&text_out.stdout))
}

fn model_from_json(v: &serde_json::Value) -> LoadedModel {
    let identifier = v
        .get("identifier")
        .or_else(|| v.get("id"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let model = v
        .get("modelKey")
        .or_else(|| v.get("model"))
        .or_else(|| v.get("id"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let status = v
        .get("status")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    // Real `lms ps --json` reports `sizeBytes` (integer) — the string
    // `size` field is only present in older / text-shimmed payloads.
    // Format bytes to decimal GB (LMStudio's text-output convention)
    // so downstream parsers see a consistent "X.XX GB" representation.
    let size = v
        .get("size")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .or_else(|| {
            v.get("sizeBytes")
                .and_then(|x| x.as_u64())
                .map(|b| format!("{:.2} GB", b as f64 / 1_000_000_000.0))
        })
        .unwrap_or_default();
    let context = v
        .get("contextLength")
        .or_else(|| v.get("context"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    LoadedModel {
        identifier,
        model,
        status,
        size,
        context,
    }
}

fn parse_text_ps(text: &str) -> Vec<LoadedModel> {
    let mut out: Vec<LoadedModel> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("IDENTIFIER") {
            continue;
        }
        // columns separated by 2+ spaces
        let cols: Vec<&str> = trimmed
            .split("  ")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
        if cols.len() < 5 {
            continue;
        }
        let context = cols[4].parse::<u64>().unwrap_or(0);
        out.push(LoadedModel {
            identifier: cols[0].to_string(),
            model: if cols.len() > 1 { cols[1].to_string() } else { cols[0].to_string() },
            status: cols.get(2).copied().unwrap_or("").to_string(),
            size: cols.get(3).copied().unwrap_or("").to_string(),
            context,
        });
    }
    out
}

/// One row from `lms ls --json` — every model the LMStudio catalog knows
/// about (downloaded), regardless of whether it's currently loaded. Used by
/// `darkmux scan` to discover models the user could add to their profile
/// registry.
///
/// `publisher` is read from `lms ls --json` (e.g. "Qwen", "google",
/// "lmstudio-community"). Surfaced through this struct as public API
/// for downstream tools; the current `scan` command consumes other
/// fields, hence the dead-code lint.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ModelMeta {
    pub model_key: String,
    pub display_name: String,
    pub publisher: String,
    pub size_bytes: u64,
    pub params_string: Option<String>,
    pub architecture: Option<String>,
    pub max_context_length: Option<u32>,
    pub trained_for_tool_use: bool,
    /// Type per LMStudio: "llm", "embedding", etc. We typically filter to
    /// `"llm"` since profiles are for chat/agentic dispatch.
    pub model_type: String,
}

/// Enumerate all models LMStudio has on disk (catalog), via `lms ls --json`.
/// Returns an empty vec on failure rather than erroring — the caller likely
/// wants to render "(no models found)" rather than crash.
pub fn list_available() -> Result<Vec<ModelMeta>> {
    let mut cmd = Command::new(lms_bin());
    cmd.args(["ls", "--json"]);
    let out = run_bounded(cmd, "ls", Deadline(DEFAULT_LIST_BOUND), StdoutMode::Capture)
        .map_err(|e| anyhow::anyhow!("running `lms ls --json`: {e}"))?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&out.stdout) else {
        return Ok(Vec::new());
    };
    let Some(arr) = parsed.as_array() else {
        return Ok(Vec::new());
    };
    Ok(arr.iter().filter_map(meta_from_json).collect())
}

fn meta_from_json(v: &serde_json::Value) -> Option<ModelMeta> {
    let model_key = v.get("modelKey").and_then(|s| s.as_str())?.to_string();
    Some(ModelMeta {
        model_key,
        display_name: v
            .get("displayName")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        publisher: v
            .get("publisher")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        size_bytes: v.get("sizeBytes").and_then(|n| n.as_u64()).unwrap_or(0),
        params_string: v
            .get("paramsString")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
        architecture: v
            .get("architecture")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
        max_context_length: v
            .get("maxContextLength")
            .and_then(|n| n.as_u64())
            .map(|n| n as u32),
        trained_for_tool_use: v
            .get("trainedForToolUse")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        model_type: v
            .get("type")
            .and_then(|s| s.as_str())
            .unwrap_or("llm")
            .to_string(),
    })
}

pub fn unload(identifier: &str) -> Result<()> {
    let mut cmd = Command::new(lms_bin());
    cmd.args(["unload", identifier]);
    let out = run_bounded(cmd, "unload", resolved_load_deadline(), StdoutMode::Null)
        .map_err(|e| anyhow::anyhow!("running `lms unload {identifier}`: {e}"))?;
    if !out.status.success() {
        bail!("lms unload {identifier} failed: {}", out.stderr.trim());
    }
    Ok(())
}

/// Load a model into LMStudio under an explicit identifier. The caller is
/// responsible for deciding whether the identifier should be darkmux-namespaced
/// (see `swap::namespaced_identifier`) or pass-through for an operator-set
/// custom name.
pub fn load_with_identifier(
    model_id: &str,
    n_ctx: u32,
    identifier: &str,
    quiet: bool,
) -> Result<()> {
    let mut cmd = Command::new(lms_bin());
    cmd.args([
        "load",
        model_id,
        "--context-length",
        &n_ctx.to_string(),
        "--identifier",
        identifier,
    ]);
    if quiet {
        // (#1135) `quiet` must actually SUPPRESS. `Command` inherits the
        // parent's stdio by default, so merely *not* setting it left the
        // `lms load` progress spinner leaking to stdout — which corrupts a
        // `--json` dispatch envelope when the load runs mid-dispatch. Null
        // stdout; keep stderr inherited so a load failure is still visible.
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::inherit());
    } else {
        // inherit stdio so the user sees the loading spinner
        cmd.stdout(std::process::Stdio::inherit());
        cmd.stderr(std::process::Stdio::inherit());
    }
    // Bounded like everything else in this file, but NOT via `run_bounded`:
    // that runner pipes/nulls stdio, and this call deliberately inherits it
    // (the operator watches the load spinner; #1135 nulls stdout in quiet
    // mode to protect `--json` envelopes). Same spawn + poll + kill shape,
    // stdio left exactly as configured above.
    let deadline = resolved_load_deadline();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("running `lms load {model_id}`"))?;
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() >= deadline.0 => {
                let _ = child.kill();
                let _ = child.wait(); // reap — the kill must not leave a zombie
                bail!(
                    "lms load {model_id} timed out after {}s                      (DARKMUX_MODEL_LOAD_TIMEOUT_SECONDS to tune)",
                    deadline.0.as_secs()
                );
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("waiting on `lms load {model_id}`: {e}");
            }
        }
    };
    if !status.success() {
        bail!("lms load {model_id} failed: exit {}", status.code().unwrap_or(-1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    #[serial_test::serial]
    fn lms_bin_default_and_overridable() {
        // Combined to avoid env-var race between parallel tests.
        unsafe { std::env::remove_var("DARKMUX_LMS_BIN") };
        assert_eq!(lms_bin(), "lms");
        unsafe { std::env::set_var("DARKMUX_LMS_BIN", "/usr/local/bin/lms-custom") };
        assert_eq!(lms_bin(), "/usr/local/bin/lms-custom");
        unsafe { std::env::remove_var("DARKMUX_LMS_BIN") };
        assert_eq!(lms_bin(), "lms");
    }

    #[test]
    fn parses_json_response() {
        let v = json!({
            "identifier": "qwen3-test",
            "modelKey": "qwen3-test",
            "status": "idle",
            "size": "2.15 GB",
            "contextLength": 68000
        });
        let m = model_from_json(&v);
        assert_eq!(m.identifier, "qwen3-test");
        assert_eq!(m.model, "qwen3-test");
        assert_eq!(m.status, "idle");
        assert_eq!(m.context, 68000);
    }

    #[test]
    fn parses_json_with_id_fallback() {
        let v = json!({"id": "fallback-id", "contextLength": 1000});
        let m = model_from_json(&v);
        assert_eq!(m.identifier, "fallback-id");
        assert_eq!(m.model, "fallback-id");
        assert_eq!(m.context, 1000);
    }

    #[test]
    fn parses_json_size_bytes_to_decimal_gb() {
        // Real `lms ps --json` payload shape — `sizeBytes` integer, no
        // `size` string. Verifies the production wire format produces a
        // populated `size` field that downstream parsers can consume.
        // 12,104,297,682 bytes is gpt-oss-20b observed live on 2026-05-13.
        let v = json!({
            "identifier": "openai/gpt-oss-20b",
            "modelKey": "openai/gpt-oss-20b",
            "status": "idle",
            "sizeBytes": 12_104_297_682u64,
            "contextLength": 32768
        });
        let m = model_from_json(&v);
        assert_eq!(m.size, "12.10 GB");
        assert_eq!(m.context, 32768);
    }

    #[test]
    fn parses_json_prefers_size_string_when_both_present() {
        // Defensive: if both fields are present, the explicit string wins
        // so older shim payloads keep their pre-formatted display.
        let v = json!({
            "identifier": "x",
            "modelKey": "x",
            "status": "idle",
            "size": "5.00 GB",
            "sizeBytes": 9_999_999_999u64,
            "contextLength": 1
        });
        let m = model_from_json(&v);
        assert_eq!(m.size, "5.00 GB");
    }

    #[test]
    fn parses_json_with_missing_fields() {
        let v = json!({});
        let m = model_from_json(&v);
        assert_eq!(m.identifier, "");
        assert_eq!(m.context, 0);
    }

    #[test]
    fn parses_text_ps_output() {
        let text = "IDENTIFIER  MODEL  STATUS  SIZE  CONTEXT\nqwen3-4b  qwen3-4b  idle  2.15 GB  68000\nqwen35-mlx  qwen35-mlx  idle  18.45 GB  101000\n";
        let parsed = parse_text_ps(text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].identifier, "qwen3-4b");
        assert_eq!(parsed[0].context, 68000);
        assert_eq!(parsed[1].identifier, "qwen35-mlx");
        assert_eq!(parsed[1].context, 101000);
    }

    #[test]
    fn parse_text_ps_skips_header_and_blank() {
        let text = "\nIDENTIFIER  MODEL  STATUS  SIZE  CONTEXT\n\n";
        let parsed = parse_text_ps(text);
        assert_eq!(parsed.len(), 0);
    }

    #[test]
    fn parse_text_ps_handles_short_columns() {
        let text = "IDENTIFIER  MODEL\nbroken  row\n";
        let parsed = parse_text_ps(text);
        // 2 columns is below the 5-column threshold
        assert_eq!(parsed.len(), 0);
    }
}
