use crate::types::LoadedModel;
use anyhow::{Context, Result, bail};
use std::env;
use std::process::Command;

fn lms_bin() -> String {
    env::var("DARKMUX_LMS_BIN").unwrap_or_else(|_| "lms".to_string())
}

pub fn list_loaded() -> Result<Vec<LoadedModel>> {
    let out = Command::new(lms_bin())
        .args(["ps", "--json"])
        .output()
        .with_context(|| "running `lms ps --json`")?;
    if out.status.success() {
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
            if let Some(arr) = parsed.as_array() {
                return Ok(arr.iter().map(model_from_json).collect());
            }
        }
    }
    // fallback to text parsing
    let text_out = Command::new(lms_bin())
        .args(["ps"])
        .output()
        .with_context(|| "running `lms ps`")?;
    let text = String::from_utf8_lossy(&text_out.stdout);
    Ok(parse_text_ps(&text))
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
    let size = v
        .get("size")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
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
    let out = Command::new(lms_bin())
        .args(["ls", "--json"])
        .output()
        .with_context(|| "running `lms ls --json`")?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
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
    let out = Command::new(lms_bin())
        .args(["unload", identifier])
        .output()
        .with_context(|| format!("running `lms unload {identifier}`"))?;
    if !out.status.success() {
        bail!(
            "lms unload {identifier} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
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
    if !quiet {
        // inherit stdio so user sees the loading spinner
        cmd.stdout(std::process::Stdio::inherit());
        cmd.stderr(std::process::Stdio::inherit());
    }
    let status = cmd
        .status()
        .with_context(|| format!("running `lms load {model_id}`"))?;
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
