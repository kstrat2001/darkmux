//! Phase 4 spike: internal runtime dispatch path.
//!
//! Routes a `darkmux crew dispatch --runtime internal <role>` invocation
//! to the `darkmux-agent-spike` docker container instead of openclaw.
//! Per-dispatch container, mounted workspace, structured output collected
//! from stdout.
//!
//! **This is a spike path** — behind the explicit `--runtime internal`
//! CLI flag while the in-house runtime is being measured. Phase 5 of
//! `spike/agent-runtime/README.md` decides whether this graduates to the
//! default path.
//!
//! Deliberately simpler than the openclaw path:
//!
//! - No openclaw pre-flight (it's not involved)
//! - No `--workdir` symlink injection (workspace is a fresh tempdir
//!   per dispatch; the gallery-incident class of bug is structurally
//!   impossible because there's nowhere persistent to leak into)
//! - No sprint-output persistence (Phase 6+ design)
//! - No watched-path post-dispatch echo (same — Phase 6+)
//! - No model pin enforcement (probes whatever LMStudio currently has loaded)
//!
//! See `spike/agent-runtime/` for the runtime image this dispatches to.

use crate::crew::dispatch::DispatchResult;
use crate::crew::dispatch::DispatchOpts;
use crate::crew::loader::{load_role_prompt, load_roles};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Docker image tag for the spike runtime. Built locally from
/// `spike/agent-runtime/Dockerfile`. Phase 6 will make this configurable.
const SPIKE_IMAGE: &str = "darkmux-agent-spike:latest";

/// LMStudio /v1/models URL used to probe the currently-loaded model
/// when no explicit model is provided. Phase 4 spike uses
/// "whatever's loaded"; Phase 6 will resolve via the role pin table.
const LMSTUDIO_MODELS_URL: &str = "http://localhost:1234/v1/models";

pub fn dispatch(opts: DispatchOpts) -> Result<DispatchResult> {
    eprintln!(
        "darkmux crew dispatch: runtime=internal (spike) — image: {SPIKE_IMAGE}"
    );

    // 1. Load the role manifest + .md prompt. The internal runtime uses
    //    the SAME on-disk role definition as the openclaw path so the
    //    prompts stay identical across runtimes — that's load-bearing
    //    for Phase 5's comparison.
    let roles = load_roles().context("loading crew roles for internal dispatch")?;
    let _role = roles
        .iter()
        .find(|r| r.id == opts.role_id)
        .ok_or_else(|| anyhow!("role not found: {}", opts.role_id))?;
    let system_prompt = load_role_prompt(&opts.role_id).ok_or_else(|| {
        anyhow!(
            "role '{}' has no .md system prompt — internal runtime requires one",
            opts.role_id
        )
    })?;

    // 2. Resolve the model. Phase 4 spike: probe LMStudio for whatever's
    //    currently loaded. Phase 6 will use the role pin + active profile.
    let model = probe_loaded_model().context(
        "no model loaded in LMStudio. Load one (darkmux swap <profile>) before dispatching."
    )?;
    eprintln!("darkmux crew dispatch: model={model}");

    // 3. Resolve session id — same shape as the openclaw path so
    //    callers that compare sessions across runtimes have a stable
    //    handle.
    let unix_micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let session_id = opts.session_id.clone().unwrap_or_else(|| {
        format!(
            "crew-dispatch-{}-{unix_micros}-internal",
            opts.role_id
        )
    });

    // 4. Per-dispatch workspace tempdir. NOT auto-cleaned — leaving the
    //    workspace on disk gives the operator post-dispatch artifact
    //    visibility (which is half the point of replacing the openclaw
    //    workspace model). The path is announced on stderr.
    let workspace = std::env::temp_dir().join(format!(
        "darkmux-dispatch-{}-{unix_micros}",
        opts.role_id
    ));
    fs::create_dir_all(&workspace)
        .with_context(|| format!("creating dispatch workspace: {}", workspace.display()))?;
    eprintln!(
        "darkmux crew dispatch: workspace={} (preserved after dispatch)",
        workspace.display()
    );

    // 5. Spawn the docker container. Synchronous; stdout + stderr
    //    captured. The container runs to completion and is removed
    //    automatically (--rm).
    let mut cmd = Command::new("docker");
    cmd.arg("run")
        .arg("--rm")
        .arg("-v")
        .arg(format!("{}:/workspace", workspace.display()))
        .arg(SPIKE_IMAGE)
        .arg("run")
        .arg("--model")
        .arg(&model)
        .arg("--system")
        .arg(&system_prompt)
        .arg("--prompt")
        .arg(&opts.message);

    let output = cmd
        .output()
        .context("spawning darkmux-agent-spike container")?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    Ok(DispatchResult {
        exit_code,
        stdout,
        stderr,
        session_id,
        watched_state: Vec::new(),
    })
}

/// Shell out to curl to fetch `/v1/models` from the host's LMStudio and
/// return the first model id. Uses curl so we don't drag a Rust HTTP
/// client dep into darkmux's main crate for the spike path.
fn probe_loaded_model() -> Result<String> {
    let output = Command::new("curl")
        .args([
            "-sf",
            "-m",
            "5",
            LMSTUDIO_MODELS_URL,
        ])
        .output()
        .context("running curl to probe LMStudio")?;

    if !output.status.success() {
        bail!("LMStudio /v1/models probe failed (curl exit {})", output.status.code().unwrap_or(-1));
    }

    let body: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("parsing LMStudio /v1/models response as JSON")?;

    body["data"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|m| m["id"].as_str())
        .map(String::from)
        .ok_or_else(|| anyhow!("LMStudio /v1/models returned no models"))
}
