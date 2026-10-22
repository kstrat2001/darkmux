use crate::gestalt_host::lms_host::{run_bounded, StdoutMode, DEFAULT_LIST_BOUND};
use crate::gestalt_host::resolved_load_deadline;
use anyhow::{Result, bail};
use darkmux_gestalt::Deadline;
use crate::engine::{ModelMeta, ModelProvenance};
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

/// Every model LMStudio currently has resident, via `lms ps --json` with a
/// `lms ps` text fallback.
///
/// **"Nothing is loaded" and "I could not tell" are DISTINCT results
/// (#2774 round-9 MF3).** An `Ok(vec![])` is a positive statement that the
/// host reported zero residents; anything this function could not
/// interpret is an `Err`, never an empty success.
///
/// It used to collapse the two. Both probes fell through to
/// `Ok(<text parse of stdout>)`, and that parse yields an empty vec
/// for unrecognized text exactly as it does for a header with no rows —
/// with neither call site checking `status.success()`. Proven with a fake
/// `lms` on `PATH`: garbage stdout at exit 0, and exit 1 with no stdout,
/// each produced `Ok(vec![])`.
///
/// The consumer that makes this a safety defect rather than a cosmetic one
/// is tier 5. The thermal breaker calls `ownership::eject_all_managed` on a real
/// `critical` trip, unattended, and that function's only `Err` path is this
/// listing. A silent empty made it emit
/// `thermal.tier5_eject { ejected: [], user_loaded_count: 0 }` —
/// indistinguishable from "genuinely nothing was resident" — while every
/// managed model stayed loaded and the machine kept cooking, with
/// `thermal.tier5_eject_failed` never firing.
///
/// Fixed HERE rather than inside `eject_all_managed` because every other
/// consumer (`darkmux doctor`, the serve daemon, the telemetry sampler,
/// `main.rs`) inherits the same gap; the ones that would rather have an
/// empty than an error already say so at their own call site with
/// `unwrap_or_default()`.
///
/// **The strictness is narrow on purpose.** A legitimately empty host
/// still returns `Ok(vec![])` — including on an older `lms` with no
/// `--json` support, whose text output is empty or a bare column header.
/// See [`interpret_text_ps`] for exactly which shapes count as a definite
/// answer; widening this to "no rows parsed ⇒ error" would break that
/// case, which is the blast radius that made the narrow reading worth
/// writing down.
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
    if !text_out.status.success() {
        // (#2774 round-9 review C5) Composed here rather than through
        // `BoundedRun::exit_detail`, whose `"exited with {}: {}"` leaves a
        // dangling colon and a double space when stderr is empty — which
        // is exactly the shape of the `exit 1` case this branch exists
        // for. The status is also rendered from its CODE: `ExitStatus`'s
        // own `Display` is already "exit status: 1", so interpolating it
        // after the word "exited" read "exited with exit status: 1".
        let how = match text_out.status.code() {
            Some(code) => format!("exited with status {code}"),
            None => "was killed by a signal".to_string(),
        };
        let stderr = text_out.stderr.trim();
        let detail = if stderr.is_empty() {
            String::new()
        } else {
            format!(" ({stderr})")
        };
        bail!(
            "`lms ps --json` did not return a JSON array and `lms ps` {how}{detail} — cannot \
             tell whether any models are loaded, which is NOT the same as none being loaded. \
             Check that `{}` is a working LMStudio CLI and that LMStudio is running.",
            lms_bin(),
        );
    }
    interpret_text_ps(&text_out.stdout).ok_or_else(|| {
        anyhow::anyhow!(
            "`lms ps --json` did not return a JSON array and the output of `lms ps` was not \
             recognizable as a model listing — cannot tell whether any models are loaded, which \
             is NOT the same as none being loaded. First line was: {:?}. Check that `{}` is a \
             working LMStudio CLI.",
            text_out.stdout.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim(),
            lms_bin(),
        )
    })
}

/// `lms ps` TEXT output → `Some(rows)` when the output is a DEFINITE
/// answer, `None` when it could not be interpreted at all (#2774 round-9
/// MF3). The `None` is what [`list_loaded`] turns into an `Err` instead of
/// an empty success.
///
/// Three shapes are a definite empty, and each is here because an older
/// `lms` with no `--json` support and nothing resident really does produce
/// one of them:
///
/// - no output at all;
/// - the column header with no rows beneath it — with any PREAMBLE above
///   that header (a version banner, say) discounted, since it is not
///   evidence about what is resident (review C4);
/// - an explicit "no models …" line.
///
/// Anything else that parsed to zero rows — a stack trace, an auth prompt,
/// a truncated response, a future CLI's redesigned table — is `None`. A
/// header PLUS unparseable rows BELOW it is `None` too: the CLI is
/// recognizable but this parser did not understand what it said, which is
/// precisely the "could not tell" case.
fn interpret_text_ps(stdout: &str) -> Option<Vec<LoadedModel>> {
    let lines: Vec<&str> = stdout.lines().map(str::trim).collect();
    let header_at = lines.iter().position(|l| is_ps_header(l));
    let body: &[&str] = match header_at {
        Some(idx) => &lines[idx + 1..],
        None => &lines[..],
    };
    let non_empty: Vec<&str> = body.iter().copied().filter(|l| !l.is_empty()).collect();
    if non_empty.is_empty() {
        return if header_at.is_some() || stdout.trim().is_empty() {
            Some(Vec::new())
        } else {
            None
        };
    }
    if non_empty.iter().all(|l| l.to_ascii_lowercase().contains("no models")) {
        return Some(Vec::new());
    }

    let mut rows = Vec::new();
    for line in non_empty {
        let cols: Vec<&str> = line
            .split("  ")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
        if cols.len() < 5 {
            return None;
        }
        let context = cols[4].parse::<u64>().unwrap_or(0);
        rows.push(LoadedModel {
            identifier: cols[0].to_string(),
            model: if cols.len() > 1 { cols[1].to_string() } else { cols[0].to_string() },
            status: cols.get(2).copied().unwrap_or("").to_string(),
            size: cols.get(3).copied().unwrap_or("").to_string(),
            context,
            queued: None,
        });
    }
    Some(rows)
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
    // (#2917) Requests waiting on the instance. Absent stays `None`: an
    // older `lms` that does not report a queue has not reported zero.
    let queued = v.get("queued").and_then(|x| x.as_u64());
    LoadedModel {
        identifier,
        model,
        status,
        size,
        context,
        queued,
    }
}

/// Whether a trimmed `lms ps` line is the COLUMN HEADER rather than a
/// model row.
///
/// (#2774 round-9 review) Case-INSENSITIVE, and matching the first
/// whitespace-separated word rather than a prefix. Both halves fix a
/// pre-existing defect: `starts_with("IDENTIFIER")` let a lowercase
/// header (`identifier model status size context`) through as a
/// five-column model row, so the text parser reported one PHANTOM
/// resident — a model named "identifier" that is not loaded and cannot
/// be unloaded. Word-matching also stops a real identifier that merely
/// begins with those letters from being swallowed as a header.
fn is_ps_header(trimmed: &str) -> bool {
    trimmed
        .split_whitespace()
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("IDENTIFIER"))
}

/// Enumerate all models LMStudio has on disk (catalog), via `lms ls --json`.
/// Errors when `lms` cannot be run, exits non-zero, or prints something that
/// is not a JSON list: a failed listing is not an empty catalog (#3143).
/// A caller that wants "nothing listed" on failure says so with
/// `unwrap_or_default()`.
pub fn list_available() -> Result<Vec<ModelMeta>> {
    Ok(ls_json()?.iter().filter_map(meta_from_json).collect())
}

/// `lms ls --json`'s rows. See [`list_available`] for what is an error.
fn ls_json() -> Result<Vec<serde_json::Value>> {
    let mut cmd = Command::new(lms_bin());
    cmd.args(["ls", "--json"]);
    let out = run_bounded(cmd, "ls", Deadline(DEFAULT_LIST_BOUND), StdoutMode::Capture)
        .map_err(|e| anyhow::anyhow!("running `lms ls --json`: {e}"))?;
    // (#3143) A failed listing is an error, never an empty catalog: an empty
    // list reads downstream as "nothing is downloaded", which a broken or old
    // `lms` cannot back. Same distinction `list_loaded` draws.
    if !out.status.success() {
        let how = match out.status.code() {
            Some(code) => format!("exited with status {code}"),
            None => "was killed by a signal".to_string(),
        };
        let stderr = out.stderr.trim();
        let detail = if stderr.is_empty() { String::new() } else { format!(" ({stderr})") };
        bail!("`lms ls --json` {how}{detail}");
    }
    serde_json::from_str::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .ok_or_else(|| anyhow::anyhow!("`lms ls --json` printed something that is not a JSON list"))
}

/// Where a downloaded model's weights are and what runs it: the facts a
/// shared lab result names (`darkmux lab submit`). `Ok(None)` when LM Studio
/// does not list the model.
pub fn provenance(model_key: &str) -> Result<Option<ModelProvenance>> {
    let rows = ls_json()?;
    let models_root = dirs::home_dir().map(|h| h.join(".lmstudio").join("models"));
    let runtimes = {
        let mut cmd = Command::new(lms_bin());
        cmd.args(["runtime", "ls"]);
        run_bounded(cmd, "runtime ls", Deadline(DEFAULT_LIST_BOUND), StdoutMode::Capture)
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout)
            .unwrap_or_default()
    };
    let backends = dirs::home_dir().map(|h| h.join(".lmstudio").join("extensions").join("backends"));
    Ok(provenance_from(&rows, model_key, models_root.as_deref(), &runtimes, backends.as_deref()))
}

/// [`provenance`] over what it read: the test seam.
pub(crate) fn provenance_from(
    rows: &[serde_json::Value],
    model_key: &str,
    models_root: Option<&std::path::Path>,
    runtimes: &str,
    backends_dir: Option<&std::path::Path>,
) -> Option<ModelProvenance> {
    let row = rows.iter().find(|r| {
        r.get("modelKey").and_then(|v| v.as_str()) == Some(model_key)
            || r.get("path").and_then(|v| v.as_str()) == Some(model_key)
    })?;
    let format = row.get("format").and_then(|v| v.as_str()).unwrap_or_default();
    let backend = if format.to_ascii_lowercase().contains("safetensors") || format.to_ascii_lowercase().contains("mlx") {
        Some("MLX")
    } else if format.to_ascii_lowercase().contains("gguf") {
        Some("llama.cpp")
    } else {
        None
    };
    let quant = match row.get("quantization") {
        Some(serde_json::Value::String(q)) if !q.is_empty() => Some(q.clone()),
        Some(q @ serde_json::Value::Object(_)) => q
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| q.get("bits").and_then(|b| b.as_u64()).map(|b| format!("{b}bit"))),
        _ => None,
    };
    let weights_path = row
        .get("path")
        .and_then(|v| v.as_str())
        .zip(models_root)
        .map(|(p, root)| root.join(p));
    let runtime = backend.and_then(|b| selected_runtime(runtimes, b));
    let runtime_installed_at_ms = runtime.as_deref().zip(backends_dir).and_then(|(r, dir)| runtime_installed_at(r, dir));
    Some(ModelProvenance {
        backend: backend.map(str::to_string),
        quant,
        weights_path,
        runtime,
        runtime_installed_at_ms,
    })
}

/// The selected runtime for a backend from `lms runtime ls`: the line that
/// carries a check mark or "selected", read leniently.
pub(crate) fn selected_runtime(text: &str, backend: &str) -> Option<String> {
    let family = if backend == "MLX" { "mlx-llm" } else { "llama.cpp" };
    let selected = regex_lite_word(text);
    text.lines().zip(selected).filter(|(l, sel)| l.contains('\u{2713}') || *sel).find_map(|(line, _)| {
        let at = line.find(family)?;
        let word = line[at..].split_whitespace().next()?;
        let after = word.rsplit_once('@')?.1;
        // The leading `1.13.1` of `1.13.1-beta`, as the site's reader takes it.
        let end = after.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(after.len());
        let version = after[..end].trim_end_matches('.');
        (version.contains('.') && version.split('.').all(|p| !p.is_empty())).then(|| format!("{family} {version}"))
    })
}

/// Per line: "selected" as a whole word (not "unselected").
fn regex_lite_word(text: &str) -> Vec<bool> {
    text.lines()
        .map(|l| {
            let l = l.to_ascii_lowercase();
            l.match_indices("selected").any(|(i, _)| {
                let before = l[..i].chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
                let after = l[i + 8..].chars().next().is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
                before && after
            })
        })
        .collect()
}

/// The time of the runtime's folder under LM Studio's backends, found by its
/// exact family and version (`mlx-llm-mac-arm64-apple-metal-advsimd-1.13.1`).
fn runtime_installed_at(runtime: &str, backends_dir: &std::path::Path) -> Option<u64> {
    let (family, version) = runtime.split_once(' ')?;
    std::fs::read_dir(backends_dir).ok()?.flatten().find_map(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        (name.starts_with(&format!("{family}-")) && name.ends_with(&format!("-{version}")))
            .then(|| e.metadata().ok()?.modified().ok())
            .flatten()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
    })
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
    verify_unload_outcome(out.status.success(), &out.stderr, identifier)
}

/// Classify `lms unload` through the ONE shared classifier the gestalt host
/// uses (#3083: two classifiers had diverged on "Model Not Found"). The typed
/// [`darkmux_gestalt::HostError`] rides the `anyhow` error, so a caller that
/// treats an already-gone instance as benign asks [`crate::engine::is_not_resident`].
pub(crate) fn verify_unload_outcome(success: bool, stderr: &str, identifier: &str) -> Result<()> {
    let exit_detail = format!("of {identifier} failed: {}", stderr.trim());
    match crate::gestalt_host::lms_host::classify_unload_outcome(success, stderr, identifier, &exit_detail) {
        Some(err) => Err(anyhow::Error::new(err)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn provenance_reads_the_row_the_runtime_and_its_install_time() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("mlx-llm-mac-arm64-apple-metal-advsimd-1.13.1")).unwrap();
        let rows = vec![serde_json::json!({
            "modelKey": "qwen3-coder-30b", "path": "lmstudio-community/Qwen3-Coder-30B-MLX-4bit",
            "format": "safetensors", "quantization": {"name": "4bit", "bits": 4}
        })];
        let runtimes = "LLM ENGINE                SELECTED\nmlx-llm-mac-arm64-apple-metal-advsimd@1.12.0\nmlx-llm-mac-arm64-apple-metal-advsimd@1.13.1   \u{2713}\nllama.cpp-mac-arm64-apple-metal-advsimd@1.50.0  \u{2713}\n";
        let p = provenance_from(&rows, "qwen3-coder-30b", Some(std::path::Path::new("/m")), runtimes, Some(tmp.path())).unwrap();
        assert_eq!(p.backend.as_deref(), Some("MLX"));
        assert_eq!(p.quant.as_deref(), Some("4bit"));
        assert_eq!(p.weights_path.unwrap(), std::path::Path::new("/m/lmstudio-community/Qwen3-Coder-30B-MLX-4bit"));
        assert_eq!(p.runtime.as_deref(), Some("mlx-llm 1.13.1"));
        assert!(p.runtime_installed_at_ms.is_some());
        assert!(provenance_from(&rows, "other", None, "", None).is_none());
        assert_eq!(selected_runtime(runtimes, "llama.cpp").as_deref(), Some("llama.cpp 1.50.0"));
        assert_eq!(selected_runtime("mlx-llm-x@1.2.0\n", "MLX"), None, "nothing selected");
        assert_eq!(selected_runtime("mlx-llm-x@1.2.0 unselected\n", "MLX"), None);
        assert_eq!(selected_runtime("mlx-llm-x@1.13.1-beta (selected)\n", "MLX").as_deref(), Some("mlx-llm 1.13.1"));
    }

    use super::*;
    use crate::engine::is_not_resident;
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

    /// (#2917) LM Studio's `queued` (its CLI's `modelProcessingStateSchema
    /// = {status, queued: number}`) is read through; an absent field stays
    /// `None` — an older `lms` that reports no queue has not reported zero,
    /// and radio's busy check words those two cases differently.
    #[test]
    fn parses_json_queued_and_keeps_absent_as_not_reported() {
        let v = json!({"identifier": "darkmux:x", "modelKey": "x", "status": "generating", "queued": 2});
        assert_eq!(model_from_json(&v).queued, Some(2));
        let v = json!({"identifier": "darkmux:x", "modelKey": "x", "status": "idle", "queued": 0});
        assert_eq!(model_from_json(&v).queued, Some(0));
        let v = json!({"identifier": "darkmux:x", "modelKey": "x", "status": "idle"});
        assert_eq!(model_from_json(&v).queued, None, "absent is not zero");
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
        let parsed = interpret_text_ps(text).expect("a readable listing");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].identifier, "qwen3-4b");
        assert_eq!(parsed[0].context, 68000);
        assert_eq!(parsed[1].identifier, "qwen35-mlx");
        assert_eq!(parsed[1].context, 101000);
    }

    #[test]
    fn text_ps_skips_header_and_blank() {
        let text = "\nIDENTIFIER  MODEL  STATUS  SIZE  CONTEXT\n\n";
        assert_eq!(interpret_text_ps(text), Some(Vec::new()));
    }

    #[test]
    fn text_ps_short_columns_are_could_not_tell() {
        let text = "IDENTIFIER  MODEL\nbroken  row\n";
        // 2 columns is below the 5-column threshold: "could not tell", not "none".
        assert_eq!(interpret_text_ps(text), None);
    }

    /// (#3083 review) The production parser, mixed rows: one good row beside a
    /// short one is NOT a partial listing. A silently dropped resident would
    /// read as "not loaded" to the eject sweep, so the whole listing is
    /// "could not tell".
    #[test]
    fn a_good_row_beside_a_short_row_is_could_not_tell() {
        let text = "IDENTIFIER  MODEL  STATUS  SIZE  CONTEXT\n\
                    darkmux:qwen3-4b  qwen3-4b  idle  2.15 GB  68000\n\
                    qwen3-4b  qwen\n";
        assert_eq!(interpret_text_ps(text), None);
    }

    // ─── (#2774 round-9 MF3) "nothing loaded" vs "could not tell" ──────
    //
    // `interpret_text_ps` on a headed table returns an empty vec for BOTH, which is fine
    // for a parser and fatal for a safety path — tier 5's unattended
    // eject had no way to distinguish them. `interpret_text_ps` is where
    // the distinction lives; these pin both halves, because a guard that
    // only ever answers "could not tell" would pass the `None` cases and
    // break every legitimately-empty host.

    #[test]
    fn a_definite_empty_listing_is_still_a_success() {
        for (label, text) in [
            ("no output at all", ""),
            ("whitespace only", "\n  \n"),
            ("the header with no rows", "IDENTIFIER  MODEL  STATUS  SIZE  CONTEXT\n"),
            ("an explicit no-models line", "No models are currently loaded.\n"),
        ] {
            assert_eq!(
                interpret_text_ps(text),
                Some(Vec::new()),
                "{label} is a POSITIVE statement that nothing is resident — an older `lms` with \
                 no --json support produces exactly this"
            );
        }
    }

    #[test]
    fn a_real_listing_is_still_parsed() {
        let text = "IDENTIFIER  MODEL  STATUS  SIZE  CONTEXT\ndarkmux:qwen3-4b  qwen3-4b  idle  2.15 GB  68000\n";
        let rows = interpret_text_ps(text).expect("a recognizable listing is a definite answer");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].identifier, "darkmux:qwen3-4b");
    }

    /// The MODEL column is the model key, read apart from the IDENTIFIER: a
    /// darkmux load names its instance `darkmux:<key>`, so collapsing the two
    /// would report the namespaced identifier as the model.
    #[test]
    fn the_model_column_is_read_apart_from_the_identifier() {
        let text = "IDENTIFIER  MODEL  STATUS  SIZE  CONTEXT\ndarkmux:qwen3-4b  qwen3-4b  idle  2.15 GB  68000\n";
        let rows = interpret_text_ps(text).expect("a readable listing");
        assert_eq!(
            (rows[0].identifier.as_str(), rows[0].model.as_str(), rows[0].status.as_str(), rows[0].size.as_str()),
            ("darkmux:qwen3-4b", "qwen3-4b", "idle", "2.15 GB")
        );
    }

    /// (#2774 round-9 review C4) The narrow reading must actually cover
    /// the case it was written for. An old `lms` with no `--json` support
    /// prints its own version banner above the header; on a host with
    /// genuinely nothing loaded that read as "could not tell", so
    /// `machine eject` went rc 0 -> rc 1 on a correct answer — reachable
    /// only when `--json` also fails, which IS the old-CLI case.
    #[test]
    fn a_preamble_above_the_header_does_not_make_an_empty_listing_unreadable() {
        for (label, text) in [
            (
                "a version banner",
                "LM Studio CLI v0.3.9\nIDENTIFIER  MODEL  STATUS  SIZE  CONTEXT\n",
            ),
            (
                "a banner and a blank line",
                "LM Studio CLI v0.3.9\n\nIDENTIFIER  MODEL  STATUS  SIZE  CONTEXT\n\n",
            ),
        ] {
            assert_eq!(
                interpret_text_ps(text),
                Some(Vec::new()),
                "{label}: what sits ABOVE the header is not evidence about what is resident"
            );
        }

        // …and a preamble does NOT license ignoring a row below the
        // header this parser could not read.
        assert_eq!(
            interpret_text_ps(
                "LM Studio CLI v0.3.9\nIDENTIFIER  MODEL  STATUS  SIZE  CONTEXT\nqwen3-4b  qwen\n"
            ),
            None,
            "a preamble must not turn an unreadable row into an empty listing"
        );
    }

    /// (#2774 round-9 review) Pre-existing in the text parser, found while
    /// fixing the listing: `starts_with("IDENTIFIER")` is case-SENSITIVE,
    /// so a lowercase header parsed as a five-column model row and the
    /// listing reported one PHANTOM resident — a model named "identifier"
    /// that is not loaded and cannot be unloaded.
    #[test]
    fn a_lowercase_header_is_a_header_not_a_phantom_resident() {
        let lower = "identifier  model  status  size  context\n";
        assert_eq!(interpret_text_ps(lower), Some(Vec::new()));

        // A real row under a lowercase header is still a real row.
        let with_row = "identifier  model  status  size  context\ndarkmux:qwen3-4b  qwen3-4b  idle  2.15 GB  68000\n";
        let rows = interpret_text_ps(with_row).expect("a readable listing");
        assert_eq!(rows.len(), 1, "got {rows:?}");
        assert_eq!(rows[0].identifier, "darkmux:qwen3-4b");
    }

    #[test]
    fn output_this_parser_cannot_read_is_not_an_empty_listing() {
        for (label, text) in [
            ("a stack trace", "Error: connect ECONNREFUSED 127.0.0.1:1234\n    at TCPConnectWrap\n"),
            ("an auth prompt", "Please run `lms login` first.\n"),
            ("a redesigned table", "IDENTIFIER  MODEL\nqwen3-4b  qwen3-4b\n"),
            ("one truncated row", "IDENTIFIER  MODEL  STATUS  SIZE  CONTEXT\nqwen3-4b  qwen\n"),
        ] {
            assert_eq!(
                interpret_text_ps(text),
                None,
                "{label} must read as \"could not tell\", never as \"nothing is loaded\""
            );
        }
    }

    /// Writes a throwaway executable that impersonates `lms`, so the
    /// subprocess half of [`list_loaded`] is EXECUTED rather than reasoned
    /// about. Never touches the operator's real LMStudio — the fake is
    /// reached through `DARKMUX_LMS_BIN`, and nothing here loads or
    /// unloads anything.
    fn fake_lms(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("fake-lms");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// (#2774 round-9 MF3) The two shapes the sweep proved live, executed
    /// end to end: an `lms` that answers with garbage, and one that fails
    /// outright. Both used to return `Ok(vec![])` — an artifact reading as
    /// a clean successful sweep while every managed model stayed resident.
    ///
    /// Two notes, so neither reads as a defect in this test:
    ///
    /// - The FIRST run on a machine can take ~12s, and that is not a hang.
    ///   It spawns five real processes, and macOS rescans each
    ///   newly-written executable the first time it is exec'd; warm, the
    ///   same test finishes in ~0.3s. `run_bounded`'s own poll interval is
    ///   25ms, so nothing here waits on a deadline.
    /// - nextest may mark it `leaky`. That is `run_bounded`'s pipe-drain
    ///   threads, which it deliberately abandons after `PIPE_GRACE` rather
    ///   than blocking on — shared by every `lms` call in this crate, just
    ///   exercised five times here instead of once. Pre-existing, and out
    ///   of scope for the listing fix.
    #[test]
    #[serial_test::serial]
    fn an_lms_that_cannot_be_understood_is_an_error_not_an_empty_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var("DARKMUX_LMS_BIN").ok();

        for (label, body) in [
            ("garbage on stdout at exit 0", "echo 'not a listing'; exit 0"),
            ("a hard failure with no stdout", "exit 1"),
        ] {
            let bin = fake_lms(tmp.path(), body);
            unsafe { std::env::set_var("DARKMUX_LMS_BIN", &bin) };
            let result = list_loaded();
            assert!(
                result.is_err(),
                "{label}: must not report an empty listing as a successful answer, got {:?}",
                result.map(|r| r.len())
            );
            let msg = format!("{:#}", result.unwrap_err());
            assert!(
                msg.contains("cannot tell whether any models are loaded"),
                "{label}: the error must say WHICH of the two it is: {msg}"
            );
            // (#2774 round-9 review C5) …and it must read as a sentence.
            // `BoundedRun::exit_detail` appends ": {stderr}" with no
            // stderr to append on the `exit 1` path, which is exactly
            // this case: "exited with exit status: 1:  — cannot tell".
            assert!(
                !msg.contains(": 1:") && !msg.contains("exit status:"),
                "{label}: no dangling colon and no doubled \"exit status\": {msg}"
            );
            assert!(
                !msg.contains("  "),
                "{label}: no run of consecutive spaces in an operator-facing sentence: {msg:?}"
            );
        }

        // The inverse, through the same fake: a working `lms ps --json`
        // reporting an empty host is still a plain success. Without this
        // the guard above could be satisfied by erroring unconditionally.
        let bin = fake_lms(tmp.path(), "echo '[]'; exit 0");
        unsafe { std::env::set_var("DARKMUX_LMS_BIN", &bin) };
        assert_eq!(
            list_loaded().expect("an empty JSON array is a definite answer").len(),
            0
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LMS_BIN", v),
                None => std::env::remove_var("DARKMUX_LMS_BIN"),
            }
        }
    }

    #[test]
    fn verify_unload_outcome_accepts_clean_or_noise_stderr() {
        assert!(verify_unload_outcome(true, "", "darkmux:m").is_ok());
        assert!(verify_unload_outcome(true, "Unloading darkmux:m...", "darkmux:m").is_ok());
    }

    #[test]
    fn verify_unload_outcome_rejects_failure_stderr_or_nonzero_exit() {
        assert!(verify_unload_outcome(false, "", "darkmux:m").is_err());
        assert!(verify_unload_outcome(true, "Error: server disconnected", "darkmux:m").is_err());
    }

    /// (#3083 review) One classifier, not two: the not-found shape is the
    /// typed `HostError::NotResident` (benign to callers that downcast), the
    /// same answer `classify_unload_outcome` gives the gestalt host.
    #[test]
    fn verify_unload_outcome_types_not_found_as_not_resident() {
        for stderr in ["Model Not Found", "Cannot find a model with the identifier darkmux:m"] {
            let err = verify_unload_outcome(true, stderr, "darkmux:m").unwrap_err();
            assert!(is_not_resident(&err), "{stderr:?} must read as NotResident, got {err:#}");
        }
        let stuck = verify_unload_outcome(false, "device busy", "darkmux:m").unwrap_err();
        assert!(!is_not_resident(&stuck));
        assert!(format!("{stuck:#}").contains("device busy"), "the cause must survive: {stuck:#}");
    }
}
