//! Internal runtime dispatch path.
//!
//! Routes a `darkmux dispatch <role>` invocation to the
//! `darkmux-runtime` docker container. Per-dispatch container, mounted
//! workspace, structured output collected from stdout.
//!
//! The ONLY dispatch path as of 2.0 (#1405 removed the legacy `openclaw`
//! shell-out runtime and its `--runtime` opt-in flag).
//!
//! No `--workdir` symlink injection (workspace is a fresh tempdir per
//! dispatch); no model pin enforcement (probes whatever LMStudio currently
//! has loaded via `crew::select`, a different mechanism than the retired
//! `role-model-pins.json` table).
//!
//! See `runtime/` for the container image this dispatches to.

use crate::dispatch::DispatchResult;
use crate::dispatch::DispatchOpts;
use crate::loader::{load_autonomous_dispatch_preamble, load_role_prompt, load_roles};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Local Docker image tag for the internal runtime, built from
/// `runtime/Dockerfile` by a source checkout (`docker build -t darkmux-runtime
/// runtime/`). Preferred when present — it's the dev workflow.
pub const RUNTIME_IMAGE: &str = "darkmux-runtime:latest";

/// GHCR repository for the published runtime image (#759). When no local
/// `RUNTIME_IMAGE` exists (the common case for a `brew install` user with no
/// source checkout), darkmux pulls the version-pinned tag from here on demand.
const RUNTIME_IMAGE_GHCR_REPO: &str = "ghcr.io/kstrat2001/darkmux-runtime";

// (#839) Docker security hardening constants. The container runs untrusted LLM
// output (the agent's bash tool), so Docker is the boundary — drop all Linux
// capabilities, forbid privilege escalation, limit PIDs and memory.
/// Drop ALL Linux capabilities — the runtime needs none for its job
/// (HTTP calls to LMStudio, file reads/writes via bind mounts).
const DOCKER_CAP_DROP: &str = "ALL";
/// Prevent newpriv inside the container — even if a capability is
/// granted, the kernel blocks setuid/setgid/binary-sUID tricks.
const DOCKER_SECURITY_OPT: &str = "no-new-privileges";
/// Per-container PID limit — prevents fork-bomb style resource exhaustion
/// inside the container. 512 is a sane default for an LLM agent loop.
const DOCKER_PIDS_LIMIT: u64 = 512;
/// Per-container memory limit — prevents OOM-killing the host. 4g is a
/// sane default for an LLM agent loop that may compile/test code.
const DOCKER_MEMORY: &str = "4g";
/// TODO (#839): Run the container as non-root (`--user 1000:1000`).
/// This requires verifying that the workspace bind-mount and injected
/// runtime binary are readable/executable by uid 1000:gid 1000 inside
/// the target image. Not yet shipped because per-image uid/gid mapping
/// is unpredictable — follow-up: add `--user` once verified safe.
///
/// The version-pinned GHCR runtime-image ref for THIS binary. Pins to the
/// darkmux crate version so a `brew upgrade` pulls the matching image and a
/// skewed binary/image pair never runs (#759). Public so `darkmux doctor` can
/// show the exact ref it would pull.
pub fn ghcr_runtime_image() -> String {
    format!("{RUNTIME_IMAGE_GHCR_REPO}:{}", env!("CARGO_PKG_VERSION"))
}

/// True if `tag` is one of darkmux's own runtime images (the local dev tag or
/// any GHCR-published tag). Such images have the runtime binary baked in, so
/// they run directly with NO `--image` injection. An operator-supplied
/// `--image` (e.g. `rust:slim`) is everything else → injection.
fn is_darkmux_runtime_image(tag: &str) -> bool {
    tag == RUNTIME_IMAGE || tag.starts_with(&format!("{RUNTIME_IMAGE_GHCR_REPO}:"))
}

/// `docker images -q <tag>` prints an image id iff the image is present
/// locally (exits 0 either way; empty stdout = absent). Daemon-down is treated
/// as absent here — callers run this only after a daemon check.
fn image_present_locally(tag: &str) -> bool {
    matches!(
        Command::new("docker").args(["images", "-q", tag]).output(),
        Ok(out) if out.status.success() && !out.stdout.is_empty()
    )
}

/// `docker pull` the version-pinned GHCR runtime image (#759). Streams docker's
/// own progress to stderr so a multi-second first-dispatch pull isn't a silent
/// hang. Bails with an actionable message (auth / network / build-locally) on
/// failure.
fn pull_runtime_image(image: &str) -> Result<()> {
    eprintln!("darkmux dispatch: no local runtime image — pulling `{image}` from GHCR (one-time, #759)…");
    let status = Command::new("docker")
        .args(["pull", image])
        .status()
        .with_context(|| format!("running `docker pull {image}`"))?;
    if !status.success() {
        bail!(
            "failed to pull the runtime image `{image}` from GHCR.\n\
             Options:\n  \
             - Check network / `docker login ghcr.io` if the package is private, OR\n  \
             - Build it locally from a darkmux source checkout:\n      \
             docker build -t {RUNTIME_IMAGE} runtime/"
        );
    }
    Ok(())
}

/// Resolve + ensure a darkmux runtime image is present, pulling the
/// version-pinned GHCR image on demand if neither the local dev tag nor a
/// previously-pulled GHCR image exists (#759). Returns the ref to use. The
/// caller has already confirmed the Docker daemon is reachable.
fn ensure_darkmux_image_present() -> Result<String> {
    if image_present_locally(RUNTIME_IMAGE) {
        return Ok(RUNTIME_IMAGE.to_string());
    }
    let ghcr = ghcr_runtime_image();
    if image_present_locally(&ghcr) {
        return Ok(ghcr);
    }
    pull_runtime_image(&ghcr)?;
    Ok(ghcr)
}

/// Add the two host→container bind mounts to the docker run command:
/// the agent's `/workspace` and the runtime's out-of-band bookkeeping
/// dir at `/darkmux-out`. Both mount-point literals are duplicated here
/// (not shared from the runtime crate) by necessity — the runtime is
/// built INTO the Docker image, not linked against this crate, so the
/// `/darkmux-out` literal MUST be kept in sync with
/// `runtime::trajectory::RUNTIME_OUT_BASE` by hand.
///
/// Extracted from the docker-spawn site so the mount-translation rule is
/// unit-testable without spawning a container (same rationale as
/// `apply_compaction_flags`).
pub(crate) fn apply_volume_mounts(args: &mut Vec<String>, workspace: &Path, host_out: &Path) {
    args.push("-v".to_string());
    args.push(format!("{}:/workspace", workspace.display()));
    args.push("-v".to_string());
    args.push(format!("{}:/darkmux-out", host_out.display()));
}

/// (#386) Filename the host writes the user message into (under `host_out`), and
/// the matching container path the runtime reads it from via `--prompt-file`.
/// Keeps a substantial brief off the `docker run` argv (ARG_MAX + `ps`).
/// NOTE: `PROMPT_FILE_CONTAINER_PATH` embeds the `/darkmux-out` mount literal
/// from `apply_volume_mounts` above — if that mount point ever changes, this
/// (and the `RUNTIME_OUT_BASE` sync the mount comment names) must change too.
pub(crate) const PROMPT_FILE_NAME: &str = ".prompt.txt";
const PROMPT_FILE_CONTAINER_PATH: &str = "/darkmux-out/.prompt.txt";


/// (#703) Inject the host-cached static runtime binary into an operator
/// image: bind-mount it read-only at `/darkmux-runtime` and override the
/// container entrypoint to it. Used when dispatching into an image OTHER
/// than the default (which has the binary baked in). MUST be applied after
/// the volume mounts and before the image arg — `-v` / `--entrypoint` are
/// docker-run OPTIONS that precede the IMAGE. Unit-testable without docker.
pub(crate) fn apply_runtime_injection(args: &mut Vec<String>, binary: &Path) {
    args.push("-v".to_string());
    args.push(format!("{}:/darkmux-runtime:ro", binary.display()));
    args.push("--entrypoint".to_string());
    args.push("/darkmux-runtime".to_string());
}

/// (#703 Slice 3) Mount the shared toolchain cache at `/darkmux-cache` and
/// point the language package managers at it, so the inner verify loop doesn't
/// re-download deps on every dispatch. The registry/download caches are
/// concurrency-safe; per-dispatch `target/` stays in the workspace, so
/// concurrent dispatches don't contend on build artifacts. Docker-run OPTIONS
/// (must precede the image arg). Unit-testable without docker.
fn apply_cache_mount(args: &mut Vec<String>, cache: &Path) {
    args.push("-v".to_string());
    args.push(format!("{}:/darkmux-cache", cache.display()));
    args.push("-e".to_string());
    args.push("CARGO_HOME=/darkmux-cache/cargo".to_string());
    args.push("-e".to_string());
    args.push("npm_config_cache=/darkmux-cache/npm".to_string());
    args.push("-e".to_string());
    args.push("PIP_CACHE_DIR=/darkmux-cache/pip".to_string());
}

/// (#703) Ensure the static `darkmux-runtime` binary is extracted to the
/// host cache (`~/.darkmux/runtime/darkmux-runtime`) so it can be
/// bind-mounted into an arbitrary operator image. On a cache miss, extract
/// it from `source_image` (the resolved darkmux image — local dev tag or the
/// pulled GHCR image, #759) via `docker create` + `docker cp`. The binary is
/// musl-static, so it runs in any Linux image (verified against alpine +
/// debian). Returns the cached path.
///
/// Staleness: the cache is NOT auto-invalidated — after rebuilding/pulling a
/// new runtime image, `rm ~/.darkmux/runtime/darkmux-runtime` to refresh.
/// (#907, P2 defense-in-depth) Reject image refs that could be smuggled as a
/// docker flag or carry shell/control characters. An image ref is a positional
/// arg to `docker create`/`docker run`; a leading `-`, embedded whitespace, or
/// a newline has no place in a legitimate registry ref. The `--` fence on the
/// docker invocations already prevents flag interpretation — this keeps the
/// contract explicit rather than fence-dependent.
fn validate_image_ref(image: &str) -> Result<()> {
    if image.is_empty() {
        bail!("image ref is empty");
    }
    if image.starts_with('-') {
        bail!("image ref `{image}` starts with `-` — refusing (could be parsed as a docker flag)");
    }
    if image.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("image ref `{image}` contains whitespace or control characters — refusing");
    }
    Ok(())
}

fn ensure_runtime_binary_cached(source_image: &str) -> Result<PathBuf> {
    let dir = darkmux_types::config_access::runtime_cache_dir();
    let dest = dir.join("darkmux-runtime");
    if dest.exists() {
        return Ok(dest);
    }
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating runtime-binary cache dir {}", dir.display()))?;

    // A throwaway container is the standard `docker cp` source; clean it up
    // unconditionally afterward.
    // (#907) `--` fences the image ref as a positional arg (symmetry with the
    // run path), and we validate it first so a ref starting with `-` / control
    // chars can't be smuggled as a docker flag.
    validate_image_ref(source_image)?;
    let created = Command::new("docker")
        .args(["create", "--", source_image])
        .output()
        .context("docker create (to extract the runtime binary for --image injection)")?;
    if !created.status.success() {
        bail!(
            "docker create {source_image} failed while extracting the runtime binary: {}",
            String::from_utf8_lossy(&created.stderr).trim()
        );
    }
    let cid = String::from_utf8_lossy(&created.stdout).trim().to_string();
    // (#907) `docker create` should echo the new container id; if stdout is
    // empty we have no handle to `docker rm` it, so bail rather than proceed
    // with an un-reapable throwaway container.
    if cid.is_empty() {
        bail!("docker create returned an empty container id while extracting the runtime binary");
    }

    // Extract to a temp path then atomically rename into place. `docker cp`
    // is NOT atomic — a partial write must never be left at `dest`, or the
    // cache-hit check above would hand out a truncated binary on every later
    // dispatch. The temp is cleaned up on any failure.
    let tmp = dir.join("darkmux-runtime.partial");
    let _ = fs::remove_file(&tmp);
    let tmp_str = tmp.to_str().ok_or_else(|| {
        anyhow!(
            "runtime-binary cache temp path is not valid UTF-8: {}",
            tmp.display()
        )
    })?;
    let cp = Command::new("docker")
        .args([
            "cp",
            &format!("{cid}:/usr/local/bin/darkmux-runtime"),
            tmp_str,
        ])
        .output();
    // Best-effort cleanup of the throwaway container regardless of cp result.
    let _ = Command::new("docker").args(["rm", "-f", &cid]).output();
    let cp = cp.context("docker cp (extracting the runtime binary)")?;
    if !cp.status.success() {
        let _ = fs::remove_file(&tmp);
        bail!(
            "docker cp of the runtime binary failed: {}",
            String::from_utf8_lossy(&cp.stderr).trim()
        );
    }

    // Ensure it's executable for the bind-mount; clean up the temp on failure
    // so a non-executable partial is never promoted.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let chmod = (|| -> Result<()> {
            let mut perms = fs::metadata(&tmp)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&tmp, perms)?;
            Ok(())
        })();
        if let Err(e) = chmod {
            let _ = fs::remove_file(&tmp);
            return Err(e).with_context(|| {
                format!("chmod extracted runtime binary {}", tmp.display())
            });
        }
    }

    // Atomic publish — only a complete, executable binary ever appears at dest.
    fs::rename(&tmp, &dest)
        .with_context(|| format!("publishing runtime binary to {}", dest.display()))?;

    eprintln!(
        "darkmux dispatch: cached runtime binary → {} (from {source_image}, for --image injection)",
        dest.display()
    );
    Ok(dest)
}

/// (#368) Translate the host-side `CompactionDispatchArgs` into
/// runtime CLI flags. Each `Some(v)` becomes a `--flag v` pair on
/// the docker run command; `None` is omitted so the runtime falls
/// back to its hardcoded default for that knob. Extracted from the
/// docker-spawn site so the translation rule is unit-testable
/// without spawning a container.
fn apply_compaction_flags(
    args: &mut Vec<String>,
    compaction: &crate::dispatch::CompactionDispatchArgs,
) {
    if let Some(n) = compaction.threshold_tokens {
        args.push("--compact-threshold-tokens".to_string());
        args.push(n.to_string());
    }
    if let Some(model) = &compaction.compactor_model {
        args.push("--compactor-model".to_string());
        args.push(model.clone());
    }
    if let Some(share) = compaction.threshold_ratio {
        args.push("--compact-threshold-ratio".to_string());
        args.push(share.to_string());
    }
    if let Some(window) = compaction.context_window {
        args.push("--context-window".to_string());
        args.push(window.to_string());
    }
    // (#372 T2-C) Strategy → `--compact-strategy <kebab>`. Runtime
    // parses it back to its local enum; None ⇒ flag omitted ⇒
    // runtime uses Narrative default.
    if let Some(strategy) = compaction.strategy {
        use darkmux_types::CompactionStrategy;
        let kebab = match strategy {
            CompactionStrategy::Narrative => "narrative",
            CompactionStrategy::StructuredSlot => "structured-slot",
        };
        args.push("--compact-strategy".to_string());
        args.push(kebab.to_string());
    }
    // (#377) Escalation bound → `--bail-after-compactions N`.
    // Runtime exits with EscalationTriggered when this many
    // compactions have occurred. None ⇒ flag omitted ⇒ runtime is
    // unbounded (back-compat with pre-#377 behavior).
    if let Some(n) = compaction.bail_after_compactions {
        args.push("--bail-after-compactions".to_string());
        args.push(n.to_string());
    }
    // (#383) Operator-tunable custom instructions →
    // `--compactor-custom-instructions <text>`. Runtime appends to the
    // compactor's system prompt at compaction time. None ⇒ flag
    // omitted ⇒ runtime uses the V0 baseline system prompt.
    // Schema-isolation doctrine: this comes from the typed
    // `profile.runtime.compaction.custom_instructions` only — never
    // from `extras["customInstructions"]` (the dead-letter openclaw
    // passthrough). See DESIGN.md "Schema isolation".
    if let Some(text) = compaction.custom_instructions.as_deref() {
        args.push("--compactor-custom-instructions".to_string());
        args.push(text.to_string());
    }
}

/// (#457 Changes 2+3) Apply operator-opt-in per-dispatch caps.
/// Reads two env vars on host side; passes them to the runtime via
/// `--max-turns` / `--max-tokens` CLI flags. Both default unlimited;
/// when neither env var is set, no flag is added and the runtime's
/// `Option<u32>` parameters stay `None`.
///
/// Unparseable values fall back to "unset" (no flag added) with a
/// stderr warning rather than aborting the dispatch — keeps the
/// dispatch alive on operator-typo'd values, surfaces the issue.
fn apply_runtime_limit_flags(cmd: &mut Command) {
    // Resolve env(DARKMUX_RUNTIME_MAX_*) > config.runtime.max_* > None (#661
    // Slice 4) and emit the container flag only when a cap is set. A set-but-
    // unparseable env still warns (operator-typo help) before `config_access`
    // falls it through to the config / unset tier.
    warn_if_unparseable_u32("DARKMUX_RUNTIME_MAX_TURNS");
    warn_if_unparseable_u32("DARKMUX_RUNTIME_MAX_TOKENS");
    warn_if_unparseable_u32("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL");
    if let Some(n) = darkmux_types::config_access::max_turns() {
        cmd.arg("--max-turns").arg(n.to_string());
    }
    if let Some(n) = darkmux_types::config_access::max_tokens() {
        cmd.arg("--max-tokens").arg(n.to_string());
    }
    // (#1221) Per-call cap override — E19: the built-in 10000 truncates
    // productive reasoning on thinking-family models; benches raise it.
    if let Some(n) = darkmux_types::config_access::max_tokens_per_call() {
        cmd.arg("--max-tokens-per-call").arg(n.to_string());
    }
}


/// Configuration for building the docker-run argv vector.
/// (#842) This struct exists so `build_docker_run_argv` is a pure,
/// testable function — construct it from dispatch inputs, call once,
/// assert the full arg vector.
#[derive(Debug, Clone)]
pub struct DockerRunConfig {
    /// Unique container name for `docker ps` debugging.
    pub container_name: String,
    /// Host path of the agent's workspace tree (mounted at /workspace).
    pub workspace: PathBuf,
    /// Host path for out-of-band bookkeeping (mounted at /darkmux-out).
    pub host_out: PathBuf,
    /// Whether to inject the darkmux-runtime binary into a non-default image.
    pub inject: bool,
    /// Path to the cached runtime binary (only used when `inject` is true).
    pub runtime_binary: Option<PathBuf>,
    /// The Docker image to run (operator-supplied or resolved darkmux image).
    pub image: String,
    /// Resolved model name for the runtime CLI.
    pub model: String,
    /// Full system prompt (preamble + role prompt, specialist roles only).
    pub system_prompt: String,
    /// (#1038) The role's output JSON Schema, if it declares one. Passed to the
    /// runtime as `--response-schema` → LMStudio `response_format: json_schema`
    /// so the model is grammar-constrained. None ⇒ free-form output.
    pub output_schema: Option<serde_json::Value>,
    /// The operator's user message.
    pub message: String,
    /// Whether to request JSON envelope output from the runtime.
    pub json: bool,
    /// Allowed tools CSV (None = full catalog).
    pub allowed_tools: Option<Vec<String>>,
    /// Compaction config (may be modified by role override / utility model).
    pub compaction: crate::dispatch::CompactionDispatchArgs,
    /// Per-role feedback template overrides (empty map = no flag).
    pub feedback_templates: serde_json::Value,
    /// Host path bind-mounted at `/darkmux-cache` (the shared toolchain
    /// cache). Resolved + created at the call site so the inner verify loop
    /// reuses downloaded deps across dispatches (#703 Slice 3).
    pub cache_dir: std::path::PathBuf,
    /// (#1187) When Some, this dispatch's "brain" is a remote OpenAI-compatible
    /// endpoint rather than local LMStudio — passed to the container as
    /// `--chat-url`. Set for a role whose tool_palette grants at least one
    /// tool, OR for any role when `force_container` (#1199) opts a tool-less
    /// dispatch into the container path for bench-substrate consistency (a
    /// tool-less role otherwise stays on the light single-shot
    /// `dispatch_remote` path). The URL itself never carries a credential
    /// (auth travels in a header, not the URL — see `remote_needs_auth`), so
    /// it's safe to include in this `Debug`-derived, test-asserted struct.
    pub remote_chat_url: Option<String>,
    /// (#1187) Whether the resolved remote endpoint declares an auth mechanism.
    /// When true, `build_docker_run_argv` adds `-i` (keep stdin open) and
    /// `--auth-header-stdin`, and the caller pipes the actual secret over the
    /// container's stdin immediately after spawn (see
    /// `write_remote_auth_header_stdin`) — never via a file or env var. This
    /// flag carries no secret material itself.
    pub remote_needs_auth: bool,
    /// Override the container's `--base-url` (the LMStudio-compatible
    /// chat-completions host it dials for a LOCAL-brain dispatch — see
    /// `runtime/src/lmstudio.rs::DEFAULT_BASE_URL`). `None` leaves the flag
    /// omitted, so the runtime falls back to its baked-in
    /// `http://host.docker.internal:1234/v1` default (real LMStudio on the
    /// host). Set this to point the container at a mock chat-completions
    /// server instead — the mock-model harness's mechanism for exercising
    /// the real dispatch path with zero LMStudio/GPU involvement. Distinct
    /// from `remote_chat_url`: that field marks an agentic-REMOTE (hosted
    /// endpoint) brain and takes precedence when both are set, since
    /// `LmStudioClient::with_chat_url` overrides request routing outright
    /// (see `runtime/src/main.rs`'s client construction). Carries no secret
    /// material — safe on argv/`ps`, same as `--model`.
    pub base_url_override: Option<String>,
}

/// (#842) Build the complete `docker` command from a prepared config: the
/// program name (`docker`) at `[0]`, then the `run` subcommand + options, then
/// `--` + image + runtime CLI args. Pure function — no I/O, no side effects.
///
/// Turn this into the executable `Command` via [`docker_command_from_argv`]
/// (program = `[0]`, args = `[1..]`). Do NOT push the whole vector as arguments
/// — that runs `docker docker run …` and docker exits 125 (the #975 regression).
///
/// Order: program, `run`, OPTIONS (--rm, --name, hardening flags, mounts,
/// injection), then `--` + image + runtime CLI args.
pub fn build_docker_run_argv(config: &DockerRunConfig) -> Vec<String> {
    let mut args = Vec::new();

    // docker-run OPTIONS (must precede the image arg)
    args.push("docker".to_string());
    args.push("run".to_string());
    args.push("--rm".to_string());
    args.push("--name".to_string());
    args.push(config.container_name.clone());

    // (#839) Security hardening flags
    args.push(format!("--cap-drop={}", DOCKER_CAP_DROP));
    args.push(format!("--security-opt={}", DOCKER_SECURITY_OPT));
    args.push(format!("--pids-limit={}", DOCKER_PIDS_LIMIT));
    args.push(format!("--memory={}", DOCKER_MEMORY));

    // (#1187) Keep stdin open ONLY when the container needs its remote auth
    // secret piped in — every other dispatch (local, or a remote endpoint
    // with no auth) has no stdin use and omits this flag.
    if config.remote_needs_auth {
        args.push("-i".to_string());
    }

    // Workspace + out-dir volume mounts
    apply_volume_mounts(&mut args, &config.workspace, &config.host_out);

    // Shared toolchain cache mount (always applied). The host dir is
    // resolved + created at the call site (see DockerRunConfig.cache_dir);
    // bind it to /darkmux-cache so cargo/npm/pip caches persist across the
    // per-dispatch `--rm` container (#703 Slice 3). A bare `/darkmux-cache`
    // (no host:container colon) would be an anonymous volume discarded on
    // --rm — i.e. no caching at all.
    apply_cache_mount(&mut args, &config.cache_dir);

    // Runtime binary injection (non-default images only)
    if config.inject {
        if let Some(binary) = &config.runtime_binary {
            apply_runtime_injection(&mut args, binary);
        }
    }

    // `--` + image + runtime CLI args (everything after is the container command)
    args.push("--".to_string());
    args.push(config.image.clone());
    args.push("run".to_string());
    args.push("--model".to_string());
    args.push(config.model.clone());
    args.push("--system".to_string());
    args.push(config.system_prompt.clone());
    // (#1038) Role-declared output schema → runtime `--response-schema` →
    // LMStudio json_schema response_format (grammar-constrained output).
    if let Some(schema) = &config.output_schema {
        if let Ok(s) = serde_json::to_string(schema) {
            args.push("--response-schema".to_string());
            args.push(s);
        }
    }
    // (#386) The message goes via the out-dir mount (`--prompt-file`), NOT argv,
    // so a substantial brief can't hit ARG_MAX or show up in `ps`. The host
    // wrote it to `<host_out>/.prompt.txt` before this runs.
    args.push("--prompt-file".to_string());
    args.push(PROMPT_FILE_CONTAINER_PATH.to_string());

    // Host-side override of the container's local-brain base URL (mock-model
    // harness). Emitted whenever set — even alongside `remote_chat_url` below,
    // since `--chat-url` (when present) wins request routing in the runtime's
    // client construction (`with_chat_url` overrides `base_url` outright), so
    // there's no ordering hazard in also passing `--base-url` for the
    // compactor client, which never honors `--chat-url` (see
    // `runtime/src/main.rs`'s `compactor_client` construction).
    if let Some(url) = &config.base_url_override {
        args.push("--base-url".to_string());
        args.push(url.clone());
    }

    if config.json {
        args.push("--json".to_string());
    }

    if let Some(allowed) = config.allowed_tools.as_ref() {
        args.push("--allowed-tools".to_string());
        args.push(allowed.join(","));
    }

    // (#1187) Agentic-remote: this dispatch's brain is a remote OpenAI-compat
    // endpoint, not local LMStudio. The URL carries no secret (auth travels
    // in a header — see `remote_needs_auth` below), so it's fine directly on
    // argv/`ps`, same as `--model`.
    if let Some(url) = &config.remote_chat_url {
        args.push("--chat-url".to_string());
        args.push(url.clone());
        if config.remote_needs_auth {
            args.push("--auth-header-stdin".to_string());
        }
    }

    // Compaction flags (#368) — delegated to `apply_compaction_flags`, the
    // single source of truth. Each must stay byte-for-byte identical to the
    // runtime's accepted flag names (runtime/src/main.rs rejects an unknown
    // flag with exit 2); None ⇒ flag omitted ⇒ runtime default.
    apply_compaction_flags(&mut args, &config.compaction);

    // Feedback templates (if a non-empty object). Guarded so a non-object
    // Value can't panic this pure function.
    if config
        .feedback_templates
        .as_object()
        .is_some_and(|o| !o.is_empty())
    {
        args.push("--feedback-templates-json".to_string());
        args.push(serde_json::to_string(&config.feedback_templates).unwrap_or_default());
    }

    args
}

/// Construct the dispatch `Command` from [`build_docker_run_argv`]'s output.
/// That vector is a FULL command — the program (`docker`) at `[0]` followed by
/// its arguments — so the program comes from `argv[0]` and the arguments from
/// `argv[1..]`. Passing the whole vector as arguments was the #975 regression:
/// it ran `docker docker run …`, which docker rejects with exit 125, killing
/// every internal-runtime dispatch. `build_docker_run_argv` always emits at
/// least `["docker", "run", "--rm", …]`, so `argv[0]` is guaranteed present.
fn docker_command_from_argv(argv: &[String]) -> Command {
    debug_assert!(
        !argv.is_empty(),
        "build_docker_run_argv must emit at least the program name"
    );
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
}


/// Warn (don't abort) if a runtime-limit env var is set to a non-`u32` —
/// keeps the dispatch alive on operator typos while surfacing the issue.
/// The resolved value comes from `config_access` (which falls an unparseable
/// env through to the config / unset tier), so this is purely the typo nudge.
/// (#457 / #661 Slice 4)
fn warn_if_unparseable_u32(var: &str) {
    if let Ok(raw) = std::env::var(var) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() && trimmed.parse::<u32>().is_err() {
            eprintln!(
                "darkmux dispatch: {var}=`{raw}` is not a positive integer; \
                 ignoring it (falling through to config / runtime default). (#457)"
            );
        }
    }
}

/// Default **inactivity** timeout — kills the dispatch if no
/// compaction signal lands within this many seconds. RESETS each
/// time a compaction event fires in the trajectory, because a
/// successful compaction is observable proof the dispatch isn't
/// pathologically hung (compactor model ran, primary accepted new
/// state, turns continued).
///
/// **600s** (the `config_access` default) is the same value the prior
/// absolute deadline used. Under inactivity-reset semantics it bounds the
/// time between progress signals rather than total dispatch wall-clock —
/// dispatches making compactions every ~5-10 min stay alive
/// indefinitely up to the runtime's other bounds (per-call token
/// cap, cumulative-tokens cap, MAX_TURNS).
///
/// Resolution (#661 Slice 4): `env(DARKMUX_INACTIVITY_TIMEOUT_SECONDS) >
/// config.runtime.inactivity_timeout_seconds > 600`.
///
/// (#457) Renamed from `DEFAULT_DISPATCH_DEADLINE_SECS` /
/// `DARKMUX_RUNTIME_DEADLINE_SECONDS`. The prior absolute-deadline
/// semantics killed dispatches making observable progress (Beat 53b:
/// 88 passing tests, killed at 600s with the model still iterating).
/// Progress-signal-based limits trust empirical evidence; absolute
/// caps embed a guess about how long good work should take.
fn inactivity_timeout_seconds() -> u64 {
    darkmux_types::config_access::inactivity_timeout_seconds()
}

/// (#717, unified onto `darkmux_flow::BookendGuard` in #1230 Packet 0)
/// Bookend guard for the internal dispatch lifecycle. Once `dispatch.start`
/// is emitted, the dispatch can still `?`-return before the clean
/// `dispatch.complete` (runtime-binary extraction, container spawn, or
/// `wait_with_output` failing) — or panic. Without a terminal record that
/// leaves an orphaned start; since #714 stamped `mission_id` on it, the
/// orphan now groups under its mission and would render as perpetually
/// in-flight. This guard fires a `dispatch.error` terminal record on `Drop`
/// unless `disarm`ed, so every start has a matching terminal event. The clean
/// path (and the container-ran-but-failed path, which already emits its own
/// `dispatch.error`) calls `disarm()` after that emit, so the guard never
/// double-counts a dispatch that reached its own terminal record.
///
/// A thin wrapper: this is a flat, depth-≤1 case of the generic
/// `darkmux_flow::BookendGuard` stack (one dispatch, one open unit) — the
/// `open`/`close` bookkeeping and the Drop-time abort emission both live in
/// `darkmux-flow` now, shared with `ReviewRunGuard`
/// (`darkmux-lab::lab::review`, renamed from `FunnelBookendGuard`/
/// `darkmux-lab::lab::funnel` in #1349) and `pr_review.rs`'s
/// review→dispatch bridge (`with_dispatch_bookends`, #1349's `run_review_graph`
/// top-level wrap since retired its OWN task-level bookend — see that
/// function's doc). Emits through the process-wide default sink
/// (`darkmux_flow::record`) — same as every other record on this dispatch
/// path — so, unlike the review guards, there's no injected `ReviewEmitter`
/// to bridge and no re-lending concern (see `darkmux_flow::bookend`'s
/// module doc for why that matters elsewhere).
struct DispatchBookendGuard<'a> {
    inner: darkmux_flow::DynBookendGuard<'a>,
}

/// The unit id/kind this guard always opens/closes under — a flat guard
/// only ever has one open unit, so the id is a constant rather than
/// something derived per-call.
const DISPATCH_BOOKEND_UNIT: &str = "dispatch";

impl<'a> DispatchBookendGuard<'a> {
    fn new(
        sink: &'a mut dyn darkmux_flow::BookendSink,
        role_id: String,
        session_id: String,
        model: String,
        mission_id: Option<String>,
        phase_id: Option<String>,
    ) -> Self {
        let on_abort = move |_id: &str, _kind: &str| {
            // Best-effort, same as every other emit on this path: a
            // flow-sink write problem must not mask the original error
            // propagating out.
            crate::dispatch::build_dispatch_record_with_payload(
                darkmux_flow::Level::Error,
                "dispatch error",
                &role_id,
                &session_id,
                Some(&model),
                mission_id.as_deref(),
                phase_id.as_deref(),
                Some(serde_json::json!({
                    "runtime": "internal",
                    "result_class": "error",
                    "error": "dispatch terminated before completion (early return or panic)",
                })),
            )
        };
        Self { inner: darkmux_flow::BookendGuard::new(sink, on_abort) }
    }

    /// Arm the guard and emit `started` (the `dispatch.start` record).
    fn open(&mut self, started: darkmux_flow::FlowRecord) {
        self.inner.open(DISPATCH_BOOKEND_UNIT, DISPATCH_BOOKEND_UNIT, started);
    }

    /// Emit `finished` (the `dispatch.complete`/`dispatch.error` record)
    /// and disarm — the guard's Drop backstop never double-counts a
    /// dispatch that reached this call.
    fn close(&mut self, finished: darkmux_flow::FlowRecord) {
        self.inner.close(DISPATCH_BOOKEND_UNIT, finished);
    }

    /// Kept for a caller that emits its own terminal record through a
    /// different path and just needs to silence the Drop backstop — no
    /// production call site needs this anymore (`close()` now disarms
    /// itself), but the test suite exercises this path directly to prove
    /// the disarm-suppresses-the-backstop behavior still holds through the
    /// wrapper.
    #[allow(dead_code)]
    fn disarm(&mut self) {
        self.inner.disarm();
    }
}

/// (#888) Reclaims the auto-allocated dispatch workspace on an error/panic
/// exit, so repeated failed dispatches don't accumulate throwaway scratch
/// trees in `/tmp` (a slow disk/inode DoS). ARMED only for the auto-tempdir
/// case — an operator `--workdir` is NEVER touched (`workspace` is `None`).
/// `disarm`ed once the container has run to its terminal record, so a
/// completed dispatch's workspace is retained for inspection (status quo).
/// On the #889 wait-error path the container is explicitly killed before
/// this guard drops, so the reclaim is safe. On a PANIC mid-dispatch the
/// container may still be live (a `Child` is not killed on drop; the watchdog
/// reaps it independently via `--rm`), so the `remove_dir_all` is a
/// best-effort race there — an accepted edge for a `/tmp` scratch tree on an
/// abnormal path.
/// host_out (trajectory/metrics) is deliberately NOT cleaned even on error:
/// failed-dispatch forensics stay debuggable; only the potentially-large
/// scratch tree is reclaimed.
struct AutoWorkspaceCleanup {
    /// `Some(path)` only for an auto-allocated tempdir workspace; `None` for
    /// an operator `--workdir`, which is never cleaned.
    workspace: Option<PathBuf>,
    armed: bool,
}

impl AutoWorkspaceCleanup {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AutoWorkspaceCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(path) = &self.workspace {
            // Best-effort: a cleanup failure must not mask the original error
            // propagating out (or a panic unwinding through here).
            let _ = fs::remove_dir_all(path);
        }
    }
}

/// (#1187) Write the remote endpoint's auth header to the container's stdin as
/// `{"header": "...", "value": "..."}` and close the pipe (EOF), so
/// `runtime`'s startup read sees exactly one blob and returns — no file, no
/// env var, no FILESYSTEM artifact for a `bash`-capable model to find at any
/// point during the container's run (the secret still lives in the
/// runtime process's memory for the dispatch's duration — this closes the
/// file-based exposure, not every exposure vector; see the residual-risk
/// note in `runtime/src/main.rs` on `auth_header_stdin`). Call this
/// immediately after spawning a container built with `-i` (stdin piped);
/// the container blocks reading stdin until this returns or the pipe closes.
fn write_remote_auth_header_stdin(
    stdin: &mut std::process::ChildStdin,
    header_name: &str,
    header_value: &str,
) -> Result<()> {
    use std::io::Write;
    let body = serde_json::json!({ "header": header_name, "value": header_value }).to_string();
    stdin
        .write_all(body.as_bytes())
        .context("writing remote auth header to container stdin")
}

// ─────────────────────────────────────────────────────────────────────────
// (#1177) Light single-shot hosted-dispatch path.
//
// When a dispatch's resolved model names a REMOTE OpenAI-compatible endpoint
// (Azure OpenAI, OpenAI, a LiteLLM proxy), darkmux does NOT start the container
// runtime. It makes ONE OpenAI chat-completions call — via `curl`, the same
// convention `probe_loaded_model` uses, so no Rust HTTP-client dep is dragged
// in — and emits the same flow records the container path does, so the run
// lands in the fleet viewer identically, distinguished only by its `endpoint`
// (and by having no host-load, since the model computes off-fleet). The tier is
// NOT inferred from remoteness: a dispatched model is a worker wherever it runs,
// and a remote endpoint may serve a frontier model OR an OSS service (opencode,
// a remote vLLM) — capability is never derived from location. Single-shot only
// — no tools, no multi-turn, no compaction. The machine holding the endpoint's
// Keychain credential is the keymaster.
// ─────────────────────────────────────────────────────────────────────────

/// Resolve the selected model's `ProfileModel` (with its endpoint) WITHOUT
/// loading anything in LMStudio — so `dispatch` can branch to the hosted path
/// before the container/load machinery. `Ok(None)` ⇒ no profile model resolves
/// (the local path's `probe_loaded_model` fallback + container path).
/// `Err` ⇒ the requested (or default) profile is QUARANTINED (#1282) — a hard
/// stop: falling through here would re-resolve against a DIFFERENT profile
/// (possibly routing a dispatch to the wrong remote endpoint) before the
/// container path ever gets to raise the same error.
fn resolve_selected_profile_model(
    role: &crate::types::Role,
    profile_override: Option<&str>,
    config_path: Option<&str>,
) -> Result<Option<darkmux_types::ProfileModel>> {
    use crate::select::select_model;
    use darkmux_profiles::profiles::load_registry;
    // A registry-LOAD failure stays `Ok(None)`: the container path's
    // `resolve_dispatch_model_internal` raises the loud #1269 hard stop for
    // it, with the file named.
    let Ok(loaded) = load_registry(config_path) else {
        return Ok(None);
    };
    // (#1282) A quarantined REQUESTED profile must hard-fail with the
    // entry's own parse error, not fall into the #1054 default fallback.
    if let Some(req) = profile_override {
        if let Some(msg) = loaded.registry.quarantine_error_for(req) {
            bail!(msg);
        }
    }
    let Some((_name, profile)) = loaded.registry.resolve_active(profile_override) else {
        // (#1282) Same for a quarantined `default_profile` — without this,
        // the dispatch falls through to the container path's
        // `probe_loaded_model()` and runs against whatever LMStudio has
        // loaded instead of surfacing the broken entry.
        if let Some(default_name) = loaded.registry.default_profile.as_deref() {
            if let Some(msg) = loaded.registry.quarantine_error_for(default_name) {
                bail!(msg);
            }
        }
        return Ok(None);
    };
    let skill_index: std::collections::HashMap<String, crate::types::Skill> =
        crate::loader::load_skills()
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect();
    let Ok(id) = select_model(role, profile, |id| skill_index.get(id)) else {
        return Ok(None);
    };
    Ok(profile.models.iter().find(|m| m.id == id).cloned())
}

/// (#1187) True when a role's tool palette grants at least one tool — the
/// dividing line between the light single-shot remote path and the
/// agentic-remote container path. A tool-less role (empty `allow`) has no
/// use for a tool-calling loop regardless of backend, so it stays on the
/// cheap path; a role with `allow` entries is expected to actually explore,
/// which only the container's `loop_runner` can drive.
fn role_wants_agentic_remote(role: &crate::types::Role) -> bool {
    !role.tool_palette.allow.is_empty()
}

/// (#1199) The container-path routing decision for a remote-brained
/// dispatch: tools require it (#1187), and `force_container` opts in even a
/// tool-less role so benches get ONE consistent substrate (trajectory +
/// per-turn telemetry) regardless of where the brain runs.
fn container_path_required(role: &crate::types::Role, force_container: bool) -> bool {
    role_wants_agentic_remote(role) || force_container
}

/// (#1199) The single-shot hosted request body — pure so the cap contract
/// is unit-testable. `cap = None` keeps the historical 4096 default —
/// UNLESS `reasoning_effort` is set: reasoning tokens bill inside
/// `max_completion_tokens` (Azure/OpenAI), so a 4096 cap under high effort
/// gets consumed by invisible reasoning and returns empty content. Effort
/// without an explicit cap therefore defaults to 16384; an explicit cap
/// always wins (the operator may know their task is short).
fn single_shot_body(
    model_id: &str,
    system_prompt: &str,
    message: &str,
    cap: Option<u32>,
    reasoning_effort: Option<&str>,
) -> serde_json::Value {
    let default_cap = if reasoning_effort.is_some() { 16384 } else { 4096 };
    let mut body = serde_json::json!({
        "model": model_id,
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": message },
        ],
        // (#1177) `max_completion_tokens` is the Azure/OpenAI form (the primary
        // targets); an OpenAI-compat server that only accepts `max_tokens` would
        // reject it — a per-endpoint knob is a follow-up if that comes up.
        "max_completion_tokens": cap.unwrap_or(default_cap),
    });
    if let Some(effort) = reasoning_effort {
        body["reasoning_effort"] = serde_json::Value::String(effort.to_string());
    }
    body
}

/// (#1260) Gate a bare remote `dispatch` against the per-EXECUTION
/// remote token bucket. Per the operator's scope split, a bare dispatch
/// IS one execution (config doc: `RemoteConfig`), so its single hosted call
/// draws from a fresh per-execution allowance. A single call only "exhausts"
/// a fresh bucket when the operator has set the allowance to zero — a hard
/// opt-out — in which case the call is refused with a typed error NAMING the
/// bucket rather than dispatching off the meter; any positive allowance
/// admits the one call (the spend is then accounted in the `dispatch.complete`
/// record's `total_tokens`, spend-after). The AGENTIC-remote container path
/// (#1187 — a tool-granting role on an endpoint profile, multi-call loop) is
/// NOT metered in 1.18.0; only this single-shot path and the review pipeline's
/// seats are — see the module scope note / issue #1260 follow-up.
///
/// `pub(crate)` (#1412): `step_kinds::builtins::DispatchSingleShotStepKind`'s
/// hosted arm reuses this exact gate rather than inventing a second zero-
/// allowance check — same minimum regime, one definition of "budget 0
/// refuses." The full per-stage `RemoteBucket` regime (`darkmux-lab`'s
/// review funnel) stays out of `darkmux-crew` on purpose (dependency
/// direction: `darkmux-lab` depends on `darkmux-crew`, not the reverse) —
/// consolidating the two regimes is #1414's job, not this one's.
pub(crate) fn admit_remote_execution(budget: u64) -> Result<()> {
    if budget == 0 {
        bail!(
            "remote token budget exhausted: the per-execution allowance \
             (config.remote.max_tokens_per_execution / \
             DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION) is 0 — this hosted dispatch is \
             refused rather than run off the meter. Raise the allowance above 0 to dispatch \
             to a remote endpoint."
        );
    }
    Ok(())
}

/// If this dispatch's resolved model is REMOTE, return the pieces the hosted
/// path needs (role, system prompt, model). `Ok(None)` ⇒ local — the caller
/// falls through to the unchanged container path (which re-loads the role;
/// role load is cheap embedded-string work).
fn try_resolve_remote_target(
    opts: &DispatchOpts,
) -> Result<Option<(crate::types::Role, String, darkmux_types::ProfileModel)>> {
    let roles = load_roles().context("loading crew roles for internal dispatch")?;
    let role = match roles.iter().find(|r| r.id == opts.role_id) {
        Some(r) => r.clone(),
        None => return Ok(None), // let the main path raise the canonical "role not found"
    };
    let pm = match resolve_selected_profile_model(
        &role,
        opts.profile_name.as_deref(),
        opts.config_path.as_deref(),
    )? {
        Some(pm) if pm.endpoint.as_ref().is_some_and(|e| e.is_remote()) => pm,
        _ => return Ok(None), // local ⇒ container path
    };
    let role_prompt = load_role_prompt(&opts.role_id).ok_or_else(|| {
        anyhow!(
            "role '{}' has no .md system prompt — hosted dispatch requires one",
            opts.role_id
        )
    })?;
    let system_prompt = if role.is_specialist() {
        format!(
            "{}\n\n{}",
            load_autonomous_dispatch_preamble().trim_end(),
            role_prompt
        )
    } else {
        role_prompt
    };
    Ok(Some((role, system_prompt, pm)))
}

/// Data-boundary predicate: operator identity (`~/.darkmux/identity.md`)
/// never leaves the machine — identity augmentation applies only when the
/// dispatch's resolved brain is locally served (data-boundary decision,
/// #1405 review). `remote_brained` is true when the resolved target is a
/// remote endpoint, whether single-shot or agentic-remote container.
fn identity_augmentation_allowed(remote_brained: bool) -> bool {
    !remote_brained
}

/// The chat-completions URL: `{base}/chat/completions` (+ `?api-version=` for
/// Azure). The operator's `endpoint.url` is the base up to `/chat/completions`
/// (an Azure deployment URL, or e.g. `https://api.openai.com/v1`).
/// `pub(crate)` (#1260) — `single_shot.rs`'s hosted single-shot path reuses
/// the exact URL/auth/POST chain rather than re-deriving the Azure dialect.
pub(crate) fn remote_chat_url(ep: &darkmux_types::ModelEndpoint) -> String {
    let base = ep.base_url();
    let base = base.trim_end_matches('/');
    match ep.api_version.as_deref() {
        Some(v) => format!("{base}/chat/completions?api-version={v}"),
        None => format!("{base}/chat/completions"),
    }
}

/// A short human label for the endpoint, for the flow record payload
/// (e.g. `azure:finherogpt.cognitiveservices.azure.com/gpt-4o`). Host + the
/// model — never the full URL, never any auth. `dispatch_internal` owns
/// extracting the host from `ModelEndpoint` (`darkmux-flow` — a dependency
/// LEAF w.r.t. this crate — shouldn't know about that type); the actual
/// string formatting delegates to `darkmux_flow::remote_route_label`
/// (#1230 Packet 0) so this shape has one source of truth shared with the
/// review→dispatch bookend bridge in `src/pr_review.rs`.
fn remote_endpoint_label(ep: &darkmux_types::ModelEndpoint, model_id: &str) -> String {
    let url = ep.base_url();
    let host = url
        .split("://")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or("remote");
    darkmux_flow::remote_route_label(host, model_id)
}

/// Read the endpoint's auth secret from the Keychain and build the header
/// `(name, value)`. Read via `security find-generic-password`; NEVER logged or
/// placed on any argv. `None` ⇒ no auth (valid for an unauthenticated proxy).
/// `pub(crate)` (#1260) — reused by `single_shot.rs`'s hosted single-shot
/// path, same secret-handling discipline (Keychain read at call time, 0600
/// curl config, never on argv, never logged).
pub(crate) fn remote_auth_header(ep: &darkmux_types::ModelEndpoint) -> Result<Option<(String, String)>> {
    let Some(auth) = &ep.auth else {
        return Ok(None);
    };
    let Some(kind) = auth.auth_type else {
        return Ok(None);
    };
    // (#1312 root fix + #1311 floor) Resolve the secret via the operator-declared
    // env var (`auth.key_env`) > per-dispatch cache > bounded Keychain read
    // (`auth.keychain`). The env tier is the headless-runner escape hatch — when
    // its var is present, `security` is NEVER spawned. NEVER logs the value.
    let secret = resolve_endpoint_secret(auth)?;
    let header = match kind {
        darkmux_types::EndpointAuthType::ApiKey => ("api-key".to_string(), secret),
        darkmux_types::EndpointAuthType::Bearer => {
            ("Authorization".to_string(), format!("Bearer {secret}"))
        }
    };
    Ok(Some(header))
}

/// (#1312 — the ROOT fix for the finhub-adonisjs#563 class) Resolve an
/// endpoint's auth secret. Precedence, mirroring darkmux's other secrets
/// (`redis_url`/`serve_token`): the operator-declared env var (`auth.key_env`)
/// VERBATIM > per-dispatch in-memory cache > bounded Keychain read
/// (`auth.keychain`, #1311, via `darkmux_flow::read_keychain_bounded`).
///
/// The env tier is the escape hatch a headless runner needs — the operator
/// names WHICH variable holds the key (any provider: `OPENAI_API_KEY`,
/// `AZURE_FINHEROGPT_KEY`, …) and the CI job exports it from its secret store;
/// with the var present, `security` is NEVER spawned, so there is zero
/// keychain-read hang risk. The cache collapses the per-call reads (this runs on
/// every probe draw + judge ruling + verify) to ONE Keychain read per item per
/// process. NEVER logs the value; the `credential-read` liveness marker records
/// only the resolution TIER (`env:<var>` / `keychain:<item>`) + elapsed.
fn resolve_endpoint_secret(auth: &darkmux_types::EndpointAuth) -> Result<String> {
    // Tier 1: operator-declared env var — verbatim, no cache, no keychain. THE
    // root fix for a runner whose env already carries the key (no `security`
    // spawn ⇒ no hang). A declared-but-absent var falls through to the Keychain.
    if let Some(var) = auth.key_env.as_deref().filter(|s| !s.is_empty()) {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                darkmux_types::dispatch_liveness::liveness_detail(
                    &format!("credential-read:{var}"),
                    "endpoint-auth",
                    &format!("resolved tier=env:{var}"),
                );
                return Ok(v);
            }
        }
    }

    // Tiers 2–3 need a Keychain item. If neither a present env var nor a keychain
    // item is configured, there's no credential source — bail with both names.
    let Some(keychain) = auth.keychain.as_deref().filter(|s| !s.is_empty()) else {
        bail!(
            "endpoint auth type is set but no credential resolved: the declared env var{} is not \
             present in the environment, and no `endpoint.auth.keychain` (macOS Keychain item \
             name) is configured. Set one. Run `darkmux doctor` to see the gap.",
            auth.key_env.as_deref().map(|v| format!(" `{v}`")).unwrap_or_default()
        );
    };

    // Tier 2: per-dispatch (process-lifetime) in-memory cache. Without it a
    // single review spawns DOZENS of `security` subprocesses for the same item.
    // IN-MEMORY ONLY — the secret is already in process memory during use;
    // NEVER written to disk (that would defeat the Keychain's at-rest
    // protection + access control).
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap().get(keychain).cloned() {
        return Ok(hit);
    }

    // Tier 3: bounded Keychain read (#1311). The liveness marker BEFORE the read
    // makes a hang here visible as the last-alive phase.
    let env_hint = auth
        .key_env
        .as_deref()
        .map(|v| format!(" set + export `{v}` in the runner's env (no Keychain read needed) or"))
        .unwrap_or_else(|| " declare `endpoint.auth.key_env` and export that var, or".to_string());
    darkmux_types::dispatch_liveness::liveness_case(
        &format!("credential-read:{keychain}"),
        "endpoint-auth",
    );
    let start = Instant::now();
    let outcome =
        darkmux_flow::read_keychain_bounded(keychain, None, darkmux_flow::KEYCHAIN_READ_TIMEOUT);
    let ms = start.elapsed().as_millis();
    let secret = match outcome {
        darkmux_flow::KeychainRead::Found(v) => v.trim_end_matches('\n').to_string(),
        darkmux_flow::KeychainRead::Absent => bail!(
            "Keychain item `{keychain}` not found on this machine (this machine is the keymaster \
             for the endpoint). Add it with:\n  \
             security add-generic-password -s {keychain} -a <account> -w\n\
             Or{env_hint} provide the key that way (#1312)."
        ),
        darkmux_flow::KeychainRead::TimedOut => bail!(
            "Keychain read for `{keychain}` timed out after {}s — is the login keychain locked on \
             the runner? A keychain read should be instant; a hang here freezes the dispatch \
             before any flow record (#1311 / finhub-adonisjs#563). Unlock it \
             (`security unlock-keychain`), run on an interactive login session, or{env_hint} skip \
             the Keychain entirely (#1312).",
            darkmux_flow::KEYCHAIN_READ_TIMEOUT.as_secs()
        ),
        darkmux_flow::KeychainRead::Unavailable => bail!(
            "Could not run `security find-generic-password` to read Keychain item `{keychain}` \
             (is this macOS?). Or{env_hint} provide the key that way (#1312)."
        ),
    };
    darkmux_types::dispatch_liveness::liveness_detail(
        &format!("credential-read:{keychain}"),
        "endpoint-auth",
        &format!("resolved tier=keychain:{keychain} elapsed_ms={ms}"),
    );
    cache.lock().unwrap().insert(keychain.to_string(), secret.clone());
    Ok(secret)
}

/// (#1177 `doctor --probe`) Outcome of a live endpoint probe — everything the
/// operator needs to trust the endpoint, nothing secret.
#[derive(Debug)]
pub struct ProbeReport {
    /// The endpoint label (`azure:host/model-id` form — host + model, no auth).
    pub label: String,
    /// Round-trip wall time.
    pub wall_ms: u64,
    /// The model identifier the ENDPOINT says served the request
    /// (`response.model`). May differ from the profile's model id — Azure
    /// routes by deployment name, so this names what actually answered
    /// (the #1135 "healthy while broken" class of check).
    pub served_model: Option<String>,
    /// What the probe itself cost (`usage.total_tokens`) — tokens only,
    /// never a currency figure; surfaced so the opt-in cost is visible.
    pub total_tokens: Option<u64>,
}

/// (#1177 `doctor --probe`) Live credential/routing probe: ONE minimal chat
/// completion through the EXACT same URL/auth/POST path a real hosted
/// dispatch uses (`remote_chat_url` + `remote_auth_header` +
/// `remote_chat_completion`). Verifies the whole chain — DNS, TLS,
/// credential validity, deployment routing, api-version — not just Keychain
/// presence (which `darkmux doctor` checks offline for free). Costs a few
/// real tokens on a paid endpoint, which is why it only runs under the
/// opt-in `doctor --probe`, never by default.
pub fn probe_remote_endpoint(
    ep: &darkmux_types::ModelEndpoint,
    model_id: &str,
    timeout_seconds: u32,
) -> Result<ProbeReport> {
    let label = remote_endpoint_label(ep, model_id);
    let url = remote_chat_url(ep);
    let auth = remote_auth_header(ep)?;
    let req_body = serde_json::json!({
        "model": model_id,
        "messages": [
            { "role": "user",
              "content": "Connectivity probe from `darkmux doctor --probe`. Reply with the single word: ok" },
        ],
        // Mirrors dispatch_remote's parameter form (#1177). Small cap — the
        // probe verifies the ROUND-TRIP, not the content: a reasoning model
        // may spend the whole budget thinking and return empty content, and
        // the response shape still proves credential + routing.
        "max_completion_tokens": 64,
    });
    let t0 = SystemTime::now();
    let resp = remote_chat_completion(&url, auth.as_ref(), &req_body, timeout_seconds)?;
    let wall_ms = t0.elapsed().map(|d| d.as_millis() as u64).unwrap_or(0);
    Ok(ProbeReport {
        label,
        wall_ms,
        served_model: resp
            .get("model")
            .and_then(|m| m.as_str())
            .map(str::to_string),
        total_tokens: resp.pointer("/usage/total_tokens").and_then(|v| v.as_u64()),
    })
}

/// POST the chat-completions request via `curl`, keeping the auth secret OFF
/// the process argv (a `ps` leak vector): url + headers + body go into a 0600
/// curl config file (`-K`), removed immediately after the call.
/// Process-local counter so concurrent same-process hosted dispatches never
/// share the secret-bearing curl config filename.
static REMOTE_CFG_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Hosted-call failure, split so the retry wrapper can tell a rate limit
/// (retryable: the endpoint SAYS try later) from everything else (fail loud
/// immediately — retrying an auth error or a malformed body just burns time).
///
/// `pub(crate)` (#1222 Phase B packet 2) — `single_shot.rs` reuses the
/// classification directly in its own tests rather than duplicating the
/// error-shape corpus.
#[derive(Debug)]
pub(crate) enum HostedCallError {
    RateLimited(String),
    Other(anyhow::Error),
}

/// Backoff ladder for endpoint rate limits (free tiers especially: Gemini's
/// free tier 429s under normal sequential bench load, 2026-07-05). Attempt,
/// then retry after each delay — bounded, loud on every wait, and only for
/// 429-shaped errors.
const RATE_LIMIT_BACKOFF_SECONDS: [u64; 3] = [30, 60, 120];

/// `pub(crate)` (#1222 Phase B packet 2) — `single_shot.rs` reuses this
/// hardened curl machinery (0600 secret-bearing config file, 429 backoff)
/// for the local single-shot chat primitive.
pub(crate) fn remote_chat_completion(
    url: &str,
    auth_header: Option<&(String, String)>,
    body: &serde_json::Value,
    timeout_seconds: u32,
) -> Result<serde_json::Value> {
    let attempts = RATE_LIMIT_BACKOFF_SECONDS.len() + 1;
    let mut last = String::new();
    for (i, delay) in std::iter::once(0u64)
        .chain(RATE_LIMIT_BACKOFF_SECONDS.iter().copied())
        .enumerate()
    {
        if delay > 0 {
            eprintln!(
                "darkmux: hosted endpoint rate-limited (429) — retry {i}/{} in {delay}s",
                attempts - 1
            );
            std::thread::sleep(std::time::Duration::from_secs(delay));
        }
        match remote_chat_attempt(url, auth_header, body, timeout_seconds) {
            Ok(v) => return Ok(v),
            Err(HostedCallError::RateLimited(msg)) => last = msg,
            Err(HostedCallError::Other(e)) => return Err(e),
        }
    }
    bail!("hosted endpoint rate-limited (429) after {attempts} attempts: {last}")
}

fn remote_chat_attempt(
    url: &str,
    auth_header: Option<&(String, String)>,
    body: &serde_json::Value,
    timeout_seconds: u32,
) -> std::result::Result<serde_json::Value, HostedCallError> {
    use std::io::Write;
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    // (#1177) pid + a process-local counter — a unique name per call, so
    // parallel same-process dispatches (e.g. the review-bench) can't race the
    // same secret-bearing file.
    let n = REMOTE_CFG_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let cfg_path =
        std::env::temp_dir().join(format!("darkmux-remote-{}-{}.curl", std::process::id(), n));
    let mut cfg = String::new();
    cfg.push_str(&format!("url = \"{}\"\n", esc(url)));
    cfg.push_str("request = \"POST\"\n");
    cfg.push_str("header = \"Content-Type: application/json\"\n");
    if let Some((h, v)) = auth_header {
        cfg.push_str(&format!("header = \"{}: {}\"\n", esc(h), esc(v)));
    }
    cfg.push_str(&format!("data = \"{}\"\n", esc(&body.to_string())));
    // (#1177) Create the config 0600 ATOMICALLY (no world-readable window),
    // write the secret-bearing body, run curl — then ALWAYS remove the file,
    // even if the create/write/curl fails partway (a partial secret file must
    // never linger).
    let run = || -> Result<std::process::Output> {
        #[cfg(unix)]
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&cfg_path)
                .context("creating curl config for hosted dispatch")?
        };
        #[cfg(not(unix))]
        let mut f =
            std::fs::File::create(&cfg_path).context("creating curl config for hosted dispatch")?;
        f.write_all(cfg.as_bytes())
            .context("writing curl config body")?;
        drop(f);
        Command::new("curl")
            .args([
                "-sS",
                "-m",
                &timeout_seconds.to_string(),
                "-K",
                &cfg_path.to_string_lossy(),
            ])
            .output()
            .context("running curl for hosted dispatch")
    };
    let result = run();
    let _ = std::fs::remove_file(&cfg_path); // ALWAYS remove the secret-bearing file
    let out = result.map_err(HostedCallError::Other)?;
    if !out.status.success() {
        return Err(HostedCallError::Other(anyhow!(
            "hosted dispatch curl failed (exit {}): {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    parse_hosted_response(&out.stdout)
}

/// Classify a hosted endpoint's response body. Pure — unit-testable.
///
/// (#1177) curl (no --fail) exits 0 on HTTP 4xx/5xx — the endpoint returns
/// the error IN the body. Without this check a 401 / 429 / 400 (auth,
/// rate-limit, prompt-overflow) would parse cleanly and read as an EMPTY
/// successful review — the #1135 "healthy while broken" trap. The error may
/// be a top-level `{"error": {...}}` object (Azure/OpenAI) OR the first
/// element of an ARRAY (`[{"error": {...}}]` — Google's OpenAI-compat layer,
/// observed live 2026-07-05; the array shape previously fell through to the
/// confusing "missing choices" message). A 429 / RESOURCE_EXHAUSTED
/// classifies as retryable; every other error fails loud immediately.
/// `pub(crate)` (#1222 Phase B packet 2) — `single_shot.rs`'s tests reuse
/// this classification directly rather than re-deriving the error-shape
/// corpus (401/429/503/array-shaped/malformed/contentless).
pub(crate) fn parse_hosted_response(
    stdout: &[u8],
) -> std::result::Result<serde_json::Value, HostedCallError> {
    let head = || {
        String::from_utf8_lossy(stdout)
            .chars()
            .take(200)
            .collect::<String>()
    };
    let resp: serde_json::Value = serde_json::from_slice(stdout).map_err(|e| {
        HostedCallError::Other(anyhow!(
            "parsing hosted endpoint response as JSON: {e} (first 200 bytes: {:?})",
            head()
        ))
    })?;
    let err_obj = resp.get("error").or_else(|| {
        resp.as_array()
            .and_then(|a| a.first())
            .and_then(|f| f.get("error"))
    });
    if let Some(err) = err_obj {
        let code = err.get("code").and_then(|c| c.as_u64());
        let status = err.get("status").and_then(|s| s.as_str()).unwrap_or("");
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("(no message)");
        // Retryable = the endpoint SAYS try later: rate limits (429 /
        // RESOURCE_EXHAUSTED) and transient capacity shedding (503 /
        // UNAVAILABLE / "high demand ... try again later" — Google sheds
        // load with that message in a 200-status body, observed live
        // 2026-07-05 on the paid tier; it killed whole bench runs that a
        // 30s wait survives).
        let transient = code == Some(429)
            || code == Some(503)
            || status == "RESOURCE_EXHAUSTED"
            || status == "UNAVAILABLE"
            || (msg.contains("high demand") && msg.contains("try again"));
        if transient {
            return Err(HostedCallError::RateLimited(msg.to_string()));
        }
        return Err(HostedCallError::Other(anyhow!(
            "hosted endpoint returned an error: {msg}"
        )));
    }
    // Require the MESSAGE object, not the content field (#1222 packet 2):
    // some OpenAI-compat reasoning backends omit `content` entirely on
    // length-truncation, and both consumers already treat missing content
    // as "" (`dispatch_remote`'s `.unwrap_or("")` extraction; the probe's
    // documented empty-content-still-proves-routing contract). The #1135
    // healthy-while-broken guard stays: a body with no choices at all
    // (an error page, an unexpected shape) is still loud.
    if resp.pointer("/choices/0/message").is_none() {
        return Err(HostedCallError::Other(anyhow!(
            "hosted endpoint response missing choices[0].message (unexpected shape) \
             — first 200 bytes: {:?}",
            head()
        )));
    }
    Ok(resp)
}

/// Build a dispatch flow record for a hosted call (#1230 Packet 0: split out
/// of the former `emit_remote_record` so the bookend guard's `open`/`close`
/// can emit it instead of this function emitting directly). Same builder +
/// tier as the container path — a dispatched model is a WORKER regardless
/// of where it runs, so the tier is NOT changed. A remote endpoint may
/// serve a frontier model OR an OSS service (opencode, a remote vLLM);
/// darkmux never infers a capability tier from remoteness. The only remote
/// signal is the `endpoint` in the payload — location + service live on the
/// endpoint axis, never in the tier. Always `Level::Info`, matching the
/// original `emit_remote_record`'s behavior on every action including
/// `"dispatch error"` — a hosted-call failure surfaces via
/// `payload.result_class`/`payload.error`, not the record level.
fn build_remote_record(
    role_id: &str,
    session_id: &str,
    model: &str,
    phase_id: Option<&str>,
    action: &str,
    payload: serde_json::Value,
) -> darkmux_flow::FlowRecord {
    crate::dispatch::build_dispatch_record_with_payload(
        darkmux_flow::Level::Info,
        action,
        role_id,
        session_id,
        Some(model),
        None, // (#1177) mission_id — resolved from phase in a follow-up
        phase_id,
        Some(payload),
    )
}

/// The hosted single-shot dispatch (#1177). Precondition: `pm.endpoint` is remote.
fn dispatch_remote(
    opts: &DispatchOpts,
    _role: &crate::types::Role,
    system_prompt: &str,
    pm: &darkmux_types::ProfileModel,
) -> Result<DispatchResult> {
    let ep = pm
        .endpoint
        .as_ref()
        .expect("dispatch_remote requires a remote endpoint");
    // (#1260) Meter this bare dispatch as one execution BEFORE any record
    // is emitted — a zero allowance refuses the call cleanly, without leaving
    // an orphaned in-flight session in the viewer.
    admit_remote_execution(darkmux_types::config_access::remote_max_tokens_per_execution())?;
    let session_id = opts
        .session_id
        .clone()
        .unwrap_or_else(|| crate::dispatch::fresh_session_id(&opts.role_id));
    let label = remote_endpoint_label(ep, &pm.id);
    eprintln!(
        "darkmux dispatch: runtime=direct (hosted) — endpoint: {label} — model={}",
        pm.id
    );

    // (#1177) Resolve the URL + auth BEFORE emitting `dispatch start` — an auth
    // failure (missing Keychain item) then bails WITHOUT leaving an orphaned
    // in-flight session in the viewer.
    let url = remote_chat_url(ep);
    let auth = remote_auth_header(ep)?;
    let phase = opts.phase_id.as_deref();

    // (#1230 Packet 0) `dispatch_remote` previously had NO bookend guard at
    // all — a panic mid-hosted-call (or any future early return added
    // between `open` and the terminal records below) could orphan a
    // `dispatch start` with no terminal. Same shared guard as the container
    // path (`DispatchBookendGuard`), through the process-wide default sink.
    let mut remote_flow_sink = |r: darkmux_flow::FlowRecord| {
        let _ = darkmux_flow::record(r);
    };
    let role_id_for_abort = opts.role_id.clone();
    let session_id_for_abort = session_id.clone();
    let model_for_abort = pm.id.clone();
    let phase_for_abort = phase.map(str::to_string);
    let label_for_abort = label.clone();
    let on_abort = move |_id: &str, _kind: &str| {
        crate::dispatch::build_dispatch_record_with_payload(
            darkmux_flow::Level::Error,
            "dispatch error",
            &role_id_for_abort,
            &session_id_for_abort,
            Some(&model_for_abort),
            None,
            phase_for_abort.as_deref(),
            Some(serde_json::json!({
                "runtime": "direct",
                "endpoint": label_for_abort,
                "result_class": "error",
                "error": "dispatch terminated before completion (early return or panic)",
            })),
        )
    };
    let mut bookend = darkmux_flow::BookendGuard::new(&mut remote_flow_sink, on_abort);

    // (#1177) Cap the prompt in the record (a brief can be KB-long) — capped
    // text + full `prompt_chars`, matching the container path.
    bookend.open(
        "dispatch",
        "dispatch",
        build_remote_record(
            &opts.role_id,
            &session_id,
            &pm.id,
            phase,
            "dispatch start",
            serde_json::json!({
                "runtime": "direct",
                "endpoint": label,
                "prompt": crate::dispatch::capped_prompt(&opts.message),
                "prompt_chars": opts.message.chars().count(),
            }),
        ),
    );

    let req_body = single_shot_body(
        &pm.id,
        system_prompt,
        &opts.message,
        opts.max_completion_tokens,
        pm.endpoint
            .as_ref()
            .and_then(|e| e.reasoning_effort.as_deref()),
    );

    let t0 = SystemTime::now();
    let resp = remote_chat_completion(&url, auth.as_ref(), &req_body, opts.timeout_seconds);
    let wall_ms = t0.elapsed().map(|d| d.as_millis() as u64).unwrap_or(0);

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            bookend.close(
                "dispatch",
                build_remote_record(
                    &opts.role_id,
                    &session_id,
                    &pm.id,
                    phase,
                    "dispatch error",
                    serde_json::json!({ "runtime": "direct", "endpoint": label, "wall_ms": wall_ms, "error": e.to_string() }),
                ),
            );
            return Err(e);
        }
    };

    let content = resp
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let usage = resp.get("usage").cloned().unwrap_or(serde_json::Value::Null);
    let ptok = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let ctok = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let ttok = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(ptok + ctok);

    bookend.close(
        "dispatch",
        build_remote_record(
            &opts.role_id,
            &session_id,
            &pm.id,
            phase,
            "dispatch complete",
            serde_json::json!({
                "result_class": "ok",
                "exit_code": 0,
                "runtime": "direct",
                "endpoint": label,
                "prompt_tokens": ptok,
                "completion_tokens": ctok,
                "total_tokens": ttok,
                "total_turns": 1,
                "total_tools": 0,
                "total_compactions": 0,
                "wall_ms": wall_ms,
                "stdout_chars": content.len(),
            }),
        ),
    );

    let stdout = if opts.json {
        serde_json::to_string(&serde_json::json!({
            "result": "stop",
            "final_assistant": content,
            "metrics": {
                "model": pm.id, "endpoint": label, "runtime": "direct",
                "wall_ms": wall_ms, "turns": 1,
                "prompt_tokens": ptok, "completion_tokens": ctok, "total_tokens": ttok,
            },
        }))
        .unwrap_or_else(|_| content.clone())
    } else {
        content
    };

    Ok(DispatchResult {
        exit_code: 0,
        stdout,
        stderr: String::new(),
        session_id,
        out_dir: None,
    })
}

pub fn dispatch(opts: DispatchOpts) -> Result<DispatchResult> {
    // 0. Pre-flight: nudge the operator if the daemon isn't up. The
    //    dispatch will still write flow records to disk, but they
    //    won't be observable in the viewer until the daemon comes up.
    //    Non-blocking; the dispatch proceeds either way (#104 S3).
    darkmux_flow::daemon_probe::nudge_if_daemon_unreachable("dispatch");

    // 0.5. Licensed-adjacent ACK gate (#1405: moved here from the retired
    //      openclaw dispatch branch — this is the only dispatch path now).
    //      For roles whose prompts operate in domains regulated by
    //      professional licensure (health, law, fitness), require an
    //      operator acknowledgment on first dispatch.
    crate::dispatch::require_licensed_adjacent_ack(&opts.role_id)
        .context("licensed-adjacent role dispatch requires acknowledgment")?;

    // (#1177 / #1187) A resolved model naming a remote OpenAI-compatible
    // endpoint forks two ways, decided by whether the ROLE grants any tools:
    //
    //   - Tool-less role (e.g. `pr-reviewer`, empty `tool_palette.allow`) →
    //     the light single-shot `dispatch_remote` path: one chat-completions
    //     call, no Docker, no container. Structurally can't use tools
    //     anyway, so the heavier container path would add cost for nothing.
    //   - Tool-granting role (e.g. `code-reviewer`) → falls through to the
    //     SAME container path local dispatches use, just with the remote
    //     endpoint's URL + auth threaded into the container as its "brain"
    //     instead of local LMStudio (#1187 — agentic-remote). `agentic_pm`
    //     carries the resolved remote model forward past this point.
    let remote_target = try_resolve_remote_target(&opts)?;
    let mut agentic_pm: Option<darkmux_types::ProfileModel> = None;
    if let Some((role, system_prompt, pm)) = remote_target {
        // (#1199) `force_container` routes even a tool-less role through the
        // container/agentic path so benches get one consistent substrate
        // (trajectory + per-turn telemetry) regardless of where the brain is.
        if container_path_required(&role, opts.force_container) {
            agentic_pm = Some(pm);
        } else {
            return dispatch_remote(&opts, &role, &system_prompt, &pm);
        }
    }

    // (#703) `inject` is true only when the operator named a NON-darkmux image
    // (e.g. `rust:slim`) — then darkmux's static runtime binary is injected
    // into it (bind-mount + entrypoint override) so the coder runs in the
    // operator's environment and can compile/test in-sandbox. The default path
    // runs a darkmux image directly (binary baked in, no injection).
    let inject = opts
        .image
        .as_deref()
        .is_some_and(|img| !is_darkmux_runtime_image(img));

    // Pre-flight: Docker reachable + a darkmux runtime image present. That
    // image is needed either as the image we RUN (default path) or as the
    // SOURCE of the injected binary (--image path), so it's ensured either way
    // — pulling the version-pinned GHCR image on demand if no local image
    // exists (#759), so a `brew install` user with no source checkout can
    // dispatch. Bails loud + operator-actionable BEFORE the role-load /
    // model-probe / workspace-setup work below.
    let darkmux_image = if opts.skip_preflight {
        RUNTIME_IMAGE.to_string()
    } else {
        check_docker_preflight()?
    };

    // The image we actually run: the operator's `--image` if given, else the
    // resolved (possibly just-pulled) darkmux image.
    let image = opts.image.clone().unwrap_or_else(|| darkmux_image.clone());
    eprintln!(
        "darkmux dispatch: runtime=internal — image: {image}{}",
        if inject {
            " (darkmux-runtime binary injected)"
        } else {
            ""
        }
    );

    // 1. Load the role manifest + .md prompt.
    let roles = load_roles().context("loading crew roles for internal dispatch")?;
    let role = roles
        .iter()
        .find(|r| r.id == opts.role_id)
        .ok_or_else(|| anyhow!("role not found: {}", opts.role_id))?;
    let role_prompt = load_role_prompt(&opts.role_id).ok_or_else(|| {
        anyhow!(
            "role '{}' has no .md system prompt — internal runtime requires one",
            opts.role_id
        )
    })?;
    // (#425) Prepend the autonomous-dispatch preamble for specialist
    // roles. Utility roles (bounded-I/O transformers like mission-
    // compiler) skip it — they don't run agent loops and structurally
    // can't enter the asking-mode failure shape the preamble guards
    // against. Default (role_family unset) = specialist (preventive
    // safety: better to prepend an unneeded preamble than to miss
    // prepending a needed one).
    let system_prompt = if role.is_specialist() {
        // Trim + always insert a `\n\n` separator so operator-edited
        // override files that forgot a trailing newline still produce
        // a well-shaped joined prompt (no `...content# Role…` smashing).
        let preamble = load_autonomous_dispatch_preamble();
        format!("{}\n\n{}", preamble.trim_end(), role_prompt)
    } else {
        role_prompt
    };
    // (#147) Operator-identity augmentation (~/.darkmux/identity.md), no-op
    // when the file is absent. Data-boundary rule: operator identity never
    // leaves the machine — local dispatches only (data-boundary decision,
    // #1405 review). A dispatch whose resolved brain is a remote endpoint is
    // skipped here (the agentic-remote container path, where `agentic_pm` is
    // set) and never augmented on the single-shot `dispatch_remote` path
    // above (whose system prompt comes from `try_resolve_remote_target`,
    // which builds it without identity).
    let system_prompt = if identity_augmentation_allowed(agentic_pm.is_some()) {
        crate::dispatch::augment_prompt_with_identity(&system_prompt)
    } else {
        system_prompt
    };
    // #340 — surface unknown role-vocab tokens loudly. Unknown tokens
    // (typos like "exce" for "exec", future tokens not yet wired)
    // get silently dropped by `role_to_runtime`; without this warning
    // the operator sees `tool_palette filtered to []` and has no
    // signal about WHY their role got zero tools.
    let unknown_tokens = unknown_role_vocab_tokens(&role.tool_palette);
    if !unknown_tokens.is_empty() {
        eprintln!(
            "darkmux dispatch: role `{}` declares unknown tool-vocab tokens: [{}] \
             — these will be silently dropped from the runtime catalog (likely typos). \
             Known tokens: {}",
            opts.role_id,
            unknown_tokens.join(", "),
            known_role_vocab_csv()
        );
    }
    // Compute the runtime tool catalog from the role's tool_palette
    // (allow minus deny). When the palette is empty, returns None and
    // the runtime falls back to its full catalog (back-compat). When
    // restrictive, denied tools never reach the model — they're not in
    // the chat-completions `tools[]` field, so the model structurally
    // cannot call them. This is the runtime-side gate that prevents
    // a model from ignoring its .md doctrine and calling a denied tool
    // (the gap that let D call `edit` despite code-reviewer denying it).
    let allowed_tools = compute_runtime_allowed_tools(&role.tool_palette);

    // (#510) Validate the operator-provided `--workdir` BEFORE the model-
    // selection step below. Workdir validation is a cheap, deterministic,
    // network-free guard (symlink check + canonicalize + is_dir); model
    // selection needs LMStudio. Validating first means a bad workdir
    // (symlink escape, missing dir) fails fast with the right error
    // instead of surfacing a confusing "model selection failed" when no
    // model happens to be loaded. The canonical path is reused at
    // workspace resolution (step 4) so we validate exactly once.
    let validated_workdir = match opts.workdir.as_deref() {
        Some(custom) => Some(darkmux_types::workdir::validate_workdir(custom)?),
        None => None,
    };

    // 2. Resolve the model. (#450 / #590) `select_model(role, profile,
    //    skill_lookup)` capability-scores the role's requested vector against
    //    the profile's candidate models; with no offer vectors it falls back
    //    to the profile's default model (ModelRole removed in #601).
    //    The profile is the `--profile` override when set (#549), else the
    //    registry's `default_profile`. If no profile is configured (or has
    //    no model), falls back to `probe_loaded_model()` with a deprecation
    //    warning — back-compat for operators on the pre-refactor-1b config
    //    shape; the warning surfaces the gap so they migrate.
    //
    //    NOT the long-form probe-then-pin path documented in #408 —
    //    that's phase 2+ scope when the recommendation registry
    //    activates per-hardware tuple selection.
    // (#1187) An agentic-remote dispatch already has its model resolved (via
    // `try_resolve_remote_target`'s profile-based lookup, done up front so
    // the routing decision could be made before any container work) —
    // skip the local-probe/pin resolution entirely; there's no LMStudio
    // instance to probe when the brain is remote.
    let model = if let Some(pm) = &agentic_pm {
        pm.id.clone()
    } else {
        resolve_dispatch_model_internal(
            role,
            opts.profile_name.as_deref(),
            opts.config_path.as_deref(),
            opts.model_base_url_override.is_some(),
        )
        .context(
            "model selection failed. Ensure `~/.darkmux/profiles.json` has \
             a profile with at least one model (the default model is \
             `default_model` or the first model in `models`), or load a model in \
             LMStudio (`lms load <id>`) as a fallback."
        )?
    };
    // (#1187 follow-up) Raw label (no eprintln prefix) — this is also the value
    // that must land in `dispatch_start_payload`'s `endpoint` field below, the
    // SAME field the light single-shot `dispatch_remote` path already sets
    // (see its `label` var). Missing this was a real gap: the viewer's route
    // display (`sp.endpoint` in viewer.html) falls back to rendering
    // "LMStudio · local · this machine" whenever `endpoint` is absent — so an
    // agentic-remote dispatch that correctly ran on Azure would still show up
    // in the viewer as a local dispatch, an operator-sovereignty violation
    // (the operator has no way to tell where the model actually ran).
    let remote_endpoint_raw_label = agentic_pm
        .as_ref()
        .and_then(|pm| pm.endpoint.as_ref().map(|ep| remote_endpoint_label(ep, &pm.id)));
    eprintln!(
        "darkmux dispatch: model={model}{}",
        remote_endpoint_raw_label
            .as_deref()
            .map(|l| format!(" — brain: {l}"))
            .unwrap_or_default()
    );

    // 3. Resolve session id.
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

    // (#714) Resolve the phase → mission once so every flow record this
    // dispatch emits (start / turn / tool / compaction / complete / telemetry)
    // carries `mission_id`/`phase_id` and groups under its mission in the
    // observability view. Best-effort: None when this isn't a phase-bound
    // dispatch. The router (dispatch.rs) returns here before its own phase
    // wiring, so the internal path resolves it directly from `opts`.
    let mission_id = crate::dispatch::resolve_mission_for_phase(opts.phase_id.as_deref());
    let phase_id = opts.phase_id.clone();

    // 4. Workspace resolution. Two paths (#206):
    //
    //    a) `--workdir <path>` → mount operator's chosen path at
    //       /workspace inside the container. The path must already
    //       exist; container writes persist there post-dispatch.
    //       This is the path real engagement work uses (refactor /
    //       audit / feature dispatches against an existing repo).
    //
    //    b) No `--workdir` → allocate a fresh tempdir. Useful for
    //       toy tests, sanity probes, and one-shot dispatches that
    //       don't need persistent operator workspace state.
    //
    //    NEITHER path auto-cleans the workspace dir — the operator
    //    can inspect trajectory.jsonl + any files the agent wrote
    //    after the container exits. That's half the point of the
    //    workspace model (operator visibility into what the dispatch
    //    did).
    let workspace = match validated_workdir {
        // Already validated above (#510) — reuse the canonical path.
        // Symlink-escape guard via the shared validator (#255 Wave-E.2);
        // same scope as #227 + #232 — symlink-only, `..`-traversal is
        // operator-explicit and intentionally out of scope.
        Some(validated) => validated,
        None => {
            let auto = std::env::temp_dir().join(format!(
                "darkmux-dispatch-{}-{unix_micros}",
                opts.role_id
            ));
            fs::create_dir_all(&auto)
                .with_context(|| format!("creating dispatch workspace: {}", auto.display()))?;
            auto
        }
    };
    let workspace_source = if opts.workdir.is_some() {
        "operator-provided via --workdir"
    } else {
        "fresh tempdir (no --workdir given)"
    };
    eprintln!(
        "darkmux dispatch: workspace={} ({})",
        workspace.display(),
        workspace_source
    );

    // (#888) Reclaim the auto-allocated scratch workspace if the dispatch
    // `?`-returns or panics before the container runs to completion. Tracks
    // the tempdir ONLY for the no-`--workdir` case; an operator `--workdir`
    // is never cleaned. Disarmed at the terminal-record point below.
    let mut workspace_cleanup = AutoWorkspaceCleanup {
        workspace: if opts.workdir.is_some() {
            None
        } else {
            Some(workspace.clone())
        },
        armed: true,
    };

    // 4b. Out-of-band bookkeeping dir. The runtime writes its OWN
    //     `.darkmux-runtime/{trajectory.jsonl, metrics.json,
    //     compaction-<gen>.json}` here — SEPARATE from /workspace so a
    //     `--workdir` repo never gets a `.darkmux-runtime` dropping in
    //     the tree it's operating on. Mounted at `/darkmux-out` inside
    //     the container (see the `-v` arg below).
    //
    //     Derived from the container_name's unique micros component so
    //     two concurrent dispatches never collide. Like the workspace,
    //     this is NOT auto-cleaned — the operator inspects
    //     trajectory.jsonl + metrics.json after the container exits.
    let host_out = std::env::temp_dir().join(format!("darkmux-out-{}-{unix_micros}", opts.role_id));
    fs::create_dir_all(&host_out)
        .with_context(|| format!("creating dispatch out-dir: {}", host_out.display()))?;
    eprintln!(
        "darkmux dispatch: out-dir={} (runtime bookkeeping → /darkmux-out)",
        host_out.display()
    );

    // (#386) Write the user message to a file in the (already-mounted) out-dir
    // and hand the runtime `--prompt-file` instead of `--prompt <text>`, so a
    // substantial brief never lands on the `docker run` command line — where it
    // would both hit ARG_MAX (the very case `--message-from-file` exists for)
    // and be visible in `ps`. The container reads it back from the bind mount.
    fs::write(host_out.join(PROMPT_FILE_NAME), &opts.message)
        .with_context(|| format!("writing dispatch prompt file under {}", host_out.display()))?;

    // (#1187) Agentic-remote: resolve the auth header from Keychain (host-side,
    // same `remote_auth_header` the light single-shot path uses). Deliberately
    // held ONLY in this local variable, never written to any file or env var
    // — `bash` has no `/workspace`-escape check (unlike `read`/`write`/`edit`;
    // it's the deliberate general-purpose escape hatch), so a mounted
    // secret-bearing file is reachable by a model-issued `cat`, and an env var
    // is reachable even more trivially (`env`, visible to every process, no
    // permission gate at all). Instead, once the container spawns below, the
    // secret is piped over its STDIN — a channel the container reads exactly
    // once at startup, before the agent loop (and any chance of a tool call)
    // exists, and which leaves no artifact on any filesystem afterward.
    let mut remote_auth: Option<(String, String)> = None;
    if let Some(pm) = &agentic_pm {
        if let Some(ep) = pm.endpoint.as_ref() {
            remote_auth = remote_auth_header(ep)?;
        }
    }
    let remote_needs_auth = remote_auth.is_some();

    // 5. Emit dispatch.start flow record with runtime metadata in payload
    //    (#204). Pairs with dispatch.complete below via session_id.
    let mut dispatch_start_payload = serde_json::json!({
        "runtime": "internal",
        // (#1126) The resolved runtime image (operator `--image` or the default
        // darkmux image, line ~711) — the environment the coder ran in. The
        // viewer's run brief + recent-runs rail read `payload.image`; it was a
        // dead reference until now (no path emitted it).
        "image": image.clone(),
        "prompt_chars": opts.message.chars().count(),
        // (#1127) The dispatch prompt text (capped) — run context the viewer
        // renders collapsed. prompt_chars carries the full length.
        "prompt": crate::dispatch::capped_prompt(&opts.message),
        "system_chars": system_prompt.chars().count(),
        "workspace": workspace.display().to_string(),
    });
    // (#1187 follow-up) Mirror `dispatch_remote`'s `"endpoint": label` field —
    // its absence, not just its presence, is meaningful to the viewer (no
    // field ⇒ rendered as local LMStudio), so this must be set whenever the
    // container's brain is actually remote.
    if let Some(label) = &remote_endpoint_raw_label {
        dispatch_start_payload["endpoint"] = serde_json::json!(label);
    }
    // (#717, #1230 Packet 0) Emit dispatch.start THROUGH the bookend guard's
    // `open()` — arms it, so any `?`-return or panic before the clean
    // `dispatch.complete` below emits a `dispatch.error` terminal so the
    // start is never left orphaned (and mission-grouped-but-in-flight).
    // Emits via the process-wide default sink (`darkmux_flow::record`),
    // same as every other record on this path.
    let mut dispatch_flow_sink =
        |r: darkmux_flow::FlowRecord| {
            let _ = darkmux_flow::record(r);
        };
    let mut bookend = DispatchBookendGuard::new(
        &mut dispatch_flow_sink,
        opts.role_id.clone(),
        session_id.clone(),
        model.clone(),
        mission_id.clone(),
        phase_id.clone(),
    );
    bookend.open(crate::dispatch::build_dispatch_record_with_payload(
        darkmux_flow::Level::Info,
        "dispatch start",
        &opts.role_id,
        &session_id,
        Some(&model),
        mission_id.as_deref(),
        phase_id.as_deref(),
        Some(dispatch_start_payload),
    ));
    let dispatch_start_instant = std::time::Instant::now();

    // (#638) Session liveness heartbeat. While THIS dispatch process lives,
    // refresh a short-TTL `darkmux:session-presence:<sid>` Redis key so the
    // live fleet view keys "running" on the key's existence — a crashed /
    // killed / watchdog-timed-out dispatch (which never emits a clean
    // dispatch.complete) ages out of the live set instead of showing
    // "running" forever. Self-disables when DARKMUX_REDIS_URL is unset. The
    // TTL is the crash backstop; `stop()` after the container exits DELetes
    // the key for an instant drop on the clean path. Held to end-of-fn; an
    // early `?`-return drops it, halting the thread (key then TTLs out).
    let session_emitter = darkmux_flow::session_presence::spawn_session_emitter(
        session_id.clone(),
        Some(opts.role_id.clone()),
        Some(model.clone()),
    );

    // 6. Spawn the docker container. Async via `spawn()` (vs the older
    //    `output()`) so the live trajectory tailer (step 7) can run in
    //    parallel and emit flow records mid-dispatch — without that,
    //    topology edges go stale during long streaming turns (#231).
    //    `--rm` cleans up the container on exit.
    // (#363) Generate a unique container name so the watchdog thread
    // (below) can `docker kill` it after the wall-clock deadline.
    // Without --name, we'd have no stable handle to kill. session_id
    // is already unique-per-dispatch and reads well in `docker ps`
    // when debugging. Sanitize since docker container names allow only
    // [a-zA-Z0-9][a-zA-Z0-9_.-]*.
    let container_name = format!(
        "darkmux-dispatch-{}",
        session_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            })
            .collect::<String>()
    );

    // Build the complete docker-run argv via the pure function (#842).
    // All inputs are resolved above; this is a single deterministic call.
    let feedback_json = if let Some(templates) = role.feedback_templates.as_ref() {
        if !templates.is_empty() {
            serde_json::to_value(templates).unwrap_or(serde_json::Value::Null)
        } else {
            serde_json::Value::Null
        }
    } else {
        serde_json::Value::Null
    };

    // Resolve compaction (role override + utility model + context window).
    let mut compaction = opts.compaction.clone();
    compaction.apply_role_override(role);
    let utility_model = resolve_utility_model_internal(opts.config_path.as_deref());
    compaction.apply_utility_model(utility_model.as_deref());
    ensure_context_window(
        &mut compaction,
        resolve_context_window_internal(opts.profile_name.as_deref(), opts.config_path.as_deref())?,
    );

    // (#1280) Ensure the utility/compactor model is RESIDENT AT ITS DECLARED
    // CONTEXT — namespaced — before the container starts, exactly as the
    // dispatch model already is (#1135). Pre-#1280 this path only eprintln'd a
    // warning, and its claim that compaction "will fail if it isn't resident"
    // was wrong: LMStudio JIT-loads a downloaded-but-unloaded model on the chat
    // call, UNNAMESPACED and at the model default (~4096) — so a compaction
    // payload overflows 4096 into truncated/garbage summaries mid-long-dispatch
    // (the #1135 silent-truncation mechanism on the path that exists to SAVE
    // long dispatches), and the bare load fails `is_darkmux_owned` so `model
    // eject` can't reclaim it (a RAM leak on the always-on hub). Loading it
    // here at the compaction context window, under `darkmux:`, closes both.
    // Best-effort (warn, don't abort — a compaction-less short dispatch still
    // runs); skipped for the mock-model harness (no real LMStudio to load into,
    // same gate as the dispatch model's residency).
    if opts.model_base_url_override.is_none() {
        if let Some(warning) = ensure_utility_resident(
            compaction.compactor_model.as_deref(),
            compaction.context_window,
            ensure_model_loaded_at_ctx,
        ) {
            eprintln!("{warning}");
        }
    }

    // Shared toolchain cache: resolve the host dir + best-effort create so
    // the bind-mount target exists (#703 Slice 3). If create fails the mount
    // still works (docker creates the source) — just uncached.
    let cache_dir = darkmux_types::config_access::cache_dir();
    let _ = fs::create_dir_all(&cache_dir);

    let argv_config = DockerRunConfig {
        container_name: container_name.clone(),
        workspace: workspace.clone(),
        host_out: host_out.clone(),
        inject,
        runtime_binary: if inject {
            Some(ensure_runtime_binary_cached(&darkmux_image)?)
        } else {
            None
        },
        image: image.clone(),
        model: model.clone(),
        system_prompt: system_prompt.clone(),
        output_schema: role.output_schema.clone(),
        message: opts.message.clone(),
        json: opts.json,
        allowed_tools: allowed_tools.clone(),
        compaction: compaction.clone(),
        feedback_templates: feedback_json,
        cache_dir: cache_dir.clone(),
        remote_chat_url: agentic_pm
            .as_ref()
            .and_then(|pm| pm.endpoint.as_ref())
            .map(remote_chat_url),
        remote_needs_auth,
        base_url_override: opts.model_base_url_override.clone(),
    };

    // (#907) Validate the image ref before it reaches docker as a positional
    // arg. The `--` fence in build_docker_run_argv already prevents flag
    // interpretation; this rejects malformed refs (leading `-`, whitespace,
    // control chars) so the contract is explicit, not just fence-dependent.
    validate_image_ref(&argv_config.image)?;
    let argv = build_docker_run_argv(&argv_config);

    // `build_docker_run_argv` returns a FULL command — the program (`docker`)
    // at [0], then `run` + the #839 hardening OPTIONS + the `--` image fence +
    // runtime args. Split it: program from [0], args from [1..]. Pushing the
    // whole vector (incl. [0]) ran `docker docker run …` → docker exit 125, the
    // #975 regression. (Caps/limits/newpriv all live inside build_docker_run_argv.)
    let mut cmd = docker_command_from_argv(&argv);

    // TODO (#839): Network hardening — the runtime must reach host LMStudio
    // via `host.docker.internal`. Constraining networking (e.g. `--network none`
    // with a proxy, or `--network host` explicitly) is a separate, riskier
    // change. This phase scopes to caps/limits/user only.

    // (#457 Changes 2+3) Operator-opt-in per-dispatch caps on turn
    // count + cumulative completion tokens. Read from env vars on
    // host side; pass via --max-turns / --max-tokens CLI flags to
    // the runtime container. Both default unlimited (omitted flag
    // → runtime's `Option<u32>` stays None → no cap applied).
    apply_runtime_limit_flags(&mut cmd);

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if remote_needs_auth {
        cmd.stdin(Stdio::piped());
    }

    let mut child = cmd.spawn().context("spawning darkmux-runtime container")?;

    // (#1187) Pipe the remote auth secret over stdin IMMEDIATELY after spawn —
    // before the trajectory tailer starts, before anything else runs. The
    // container's startup code blocks reading stdin until this write (and
    // the subsequent drop, which closes the pipe / sends EOF) completes, so
    // this must happen promptly; it never touches a file or env var, so
    // there's no FILESYSTEM artifact left anywhere for a `bash`-capable
    // model to find — the whole file-based exposure the earlier design
    // (superseded within this same change) had. This does not eliminate
    // every exposure vector: the secret still lives in the runtime
    // process's memory for the dispatch's duration (see the residual-risk
    // note on `auth_header_stdin` in `runtime/src/main.rs`).
    if let Some((header_name, header_value)) = remote_auth.take() {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("container spawned with -i but stdin handle is missing"))?;
        write_remote_auth_header_stdin(&mut stdin, &header_name, &header_value)?;
        drop(stdin); // close the pipe — sends EOF, unblocking the container's read
    }

    // 7. Live trajectory tailer (#231). Background thread polls
    //    `<host_out>/.darkmux-runtime/trajectory.jsonl` every 250ms while
    //    the container runs; emits flow records in real time:
    //
    //      - `model.completed`  → `dispatch.turn`
    //      - `tool.completed`   → `dispatch.tool`
    //      - `compaction`       → `dispatch.compaction`
    //      - `model.reasoning`  → `dispatch.reasoning` (with S6 size cap)
    //      - `model.partial`    → `dispatch.turn.heartbeat` (rate-limited)
    //
    //    Best-effort: read failures are non-fatal (flow records are
    //    observability, not correctness). After the container exits, the
    //    main thread signals stop; the tailer does one final flush pass
    //    so straggler lines written between the last poll and exit are
    //    not lost.
    // (#457) Inactivity deadline shared between the trajectory tailer
    // (writes — resets on compaction events) and the watchdog (reads —
    // fires when now > deadline). `Mutex<Instant>` is the simplest
    // primitive for this 2-thread / low-contention case (compaction
    // fires every few minutes; watchdog polls every 500ms).
    //
    // Initialized to `now + inactivity_secs`; first compaction event
    // resets it forward. A dispatch that never compacts hits the
    // initial deadline. A dispatch making compactions every ~5-10 min
    // stays alive indefinitely (bounded by the other runtime caps).
    let inactivity_secs = inactivity_timeout_seconds();
    let inactivity_deadline = Arc::new(Mutex::new(
        Instant::now() + Duration::from_secs(inactivity_secs),
    ));

    let stop_flag = Arc::new(AtomicBool::new(false));
    // (#threshold) Effective compaction threshold the runtime triggers at:
    // absolute `threshold_tokens` > `threshold_ratio × window` > the 0.5×window
    // default (matching the runtime's DEFAULT_THRESHOLD_RATIO). Forwarded on
    // context telemetry so the viewer draws the trigger line.
    // Both triggers can be set; the runtime's needs_compaction fires at whichever
    // trips FIRST (min), so draw the effective earliest trigger — not just the
    // absolute one. Fall back to ratio×window, then the 0.5×window default.
    let abs_threshold = compaction.threshold_tokens;
    let formula_threshold = compaction
        .threshold_ratio
        .zip(compaction.context_window)
        .map(|(r, w)| (w as f32 * r) as u32);
    let compaction_threshold = match (abs_threshold, formula_threshold) {
        (Some(a), Some(f)) => Some(a.min(f)),
        (a, f) => a
            .or(f)
            .or_else(|| compaction.context_window.map(|w| (w as f32 * 0.5) as u32)),
    };
    let tailer_handle = {
        let stop = Arc::clone(&stop_flag);
        // (#out-of-band) The trajectory now lands in the out-dir, not the
        // workspace. The tailer reads from there.
        let out_dir = host_out.clone();
        let session_id = session_id.clone();
        let role_id = opts.role_id.clone();
        let model = model.clone();
        let mission_id = mission_id.clone();
        let phase_id = phase_id.clone();
        let inactivity_deadline = Arc::clone(&inactivity_deadline);
        thread::spawn(move || {
            run_tailer(
                out_dir,
                session_id,
                role_id,
                model,
                mission_id,
                phase_id,
                stop,
                inactivity_deadline,
                inactivity_secs,
                compaction_threshold,
            )
        })
    };

    // (#363, then #457) Inactivity watchdog. Phase B dogfood (Beat 39,
    // 2026-05-25) surfaced: thinking-mode models can hang intra-turn
    // for arbitrary wall-clock with the agent loop's MAX_TURNS cap
    // never firing. Beat 53b (2026-05-28) surfaced: an absolute
    // wall-clock deadline kills dispatches making observable progress
    // (88 passing tests written by the model, killed mid-iteration at
    // 600s). #457 reframed: trust progress signals — the deadline
    // resets each time a compaction event fires (compactor model
    // ran, primary accepted new state, dispatch is alive).
    //
    // The watchdog runs `docker kill <name>` when the *current*
    // inactivity deadline passes; the container dies with SIGKILL
    // (exit 137); the main thread's `wait_with_output()` returns;
    // the harness detects the timeout via the watchdog's atomic flag
    // + the 137 exit code and surfaces a structured
    // `dispatch.timeout` failure rather than letting the dispatch
    // hang forever.
    let timeout_fired = Arc::new(AtomicBool::new(false));
    let watchdog_done = Arc::new(AtomicBool::new(false));
    let watchdog_handle = {
        let timeout_fired = Arc::clone(&timeout_fired);
        let watchdog_done = Arc::clone(&watchdog_done);
        let container_name = container_name.clone();
        let inactivity_deadline = Arc::clone(&inactivity_deadline);
        thread::spawn(move || {
            // Poll every 500ms. Each iteration reads the CURRENT
            // deadline (which the tailer may have just reset on a
            // compaction event). When the main thread signals
            // `watchdog_done`, exit promptly without firing the kill.
            loop {
                if watchdog_done.load(Ordering::SeqCst) {
                    return;
                }
                let now = Instant::now();
                let deadline = *lock_deadline(&inactivity_deadline);
                if now >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(500));
            }
            // Race window: a natural exit can land in the final 500ms
            // sleep above. Re-check before firing to avoid stamping a
            // spurious timeout on a clean exit (QA finding 2026-05-25).
            if watchdog_done.load(Ordering::SeqCst) {
                return;
            }
            // Deadline genuinely hit before the dispatch completed.
            // Mark timeout BEFORE the kill so the post-wait detection
            // sees the flag, then SIGKILL the container.
            timeout_fired.store(true, Ordering::SeqCst);
            let _ = Command::new("docker")
                .args(["kill", &container_name])
                .output();
        })
    };

    // (#557 slice 4 · #1064) Always-on lms + host-load telemetry sampler.
    // Mirrors the tailer/watchdog lifecycle: a background thread that
    // loops until `sampler_stop` is set (after `wait_with_output()`
    // returns) + a best-effort `.join()`. Per `TELEMETRY_SAMPLE_INTERVAL`
    // it samples two surfaces and forwards each into the one flow stream
    // as a `category=telemetry` record:
    //   - `source=lms`     → load/unload deltas of the LMStudio loaded set
    //   - `source=process` → HOST system load: CPU% (top), RAM used%
    //                        (vm_stat + hw.memsize), GPU util% (ioreg). The
    //                        container is NOT sampled — inference runs in
    //                        LMStudio, off-container, so container CPU read ~0
    //                        and answered the wrong question (#814/#1064).
    // ALWAYS-ON (not `--instrument`-gated). Best-effort throughout: a failed
    // `list_loaded` / host read / record never panics or aborts the dispatch —
    // it's additive observability, so starting/stopping it is non-load-bearing.
    let sampler_stop = Arc::new(AtomicBool::new(false));
    let sampler_handle = {
        let sampler_stop = Arc::clone(&sampler_stop);
        let role_id = opts.role_id.clone();
        let session_id = session_id.clone();
        let model = model.clone();
        let mission_id = mission_id.clone();
        let phase_id = phase_id.clone();
        thread::spawn(move || {
            run_telemetry_sampler(
                sampler_stop,
                role_id,
                session_id,
                model,
                mission_id,
                phase_id,
            )
        })
    };

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            // (#889) The wait itself failed — the container may still be
            // running (orphaned). The bare `?` here used to return before
            // signaling the watchdog or killing the container: the watchdog
            // thread would linger to its inactivity deadline and fire a
            // spurious kill, and a genuinely-orphaned container would run
            // until that deadline (or forever, if hung). Do the same
            // teardown the success path does — stop the watchdog + sampler
            // threads — plus a best-effort `docker kill` by the
            // deterministic container name, then surface the wait error.
            watchdog_done.store(true, Ordering::SeqCst);
            let _ = Command::new("docker")
                .args(["kill", &container_name])
                .output();
            let _ = watchdog_handle.join();
            sampler_stop.store(true, Ordering::SeqCst);
            let _ = sampler_handle.join();
            return Err(e).context("waiting for darkmux-runtime container");
        }
    };

    // Tell the watchdog we're done so it doesn't fire spuriously after
    // a natural exit. Best-effort join (it's a kill-only thread —
    // panic-resilience isn't load-bearing).
    watchdog_done.store(true, Ordering::SeqCst);
    let _ = watchdog_handle.join();

    // (#557 slice 4) Stop the telemetry sampler now the container has
    // exited — the container's gone, so `docker stats` would just churn
    // and the loaded-model set is whatever it settled on. Mirrors the
    // tailer's stop-then-join; best-effort (observability path, so a
    // panicked sampler thread degrades to "no more samples", not a
    // failed dispatch).
    sampler_stop.store(true, Ordering::SeqCst);
    let _ = sampler_handle.join();

    // (#638) The container has exited — the session is no longer running.
    // Stop the liveness heartbeat and DELete its key so the live view drops
    // the session immediately, before the dispatch.complete record below
    // (which then renders it terminal). A crash that bypassed this path
    // leaves the key to age out via TTL instead.
    if let Some(em) = session_emitter {
        em.stop();
    }

    let wall_ms = dispatch_start_instant.elapsed().as_millis() as u64;
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // (#363) If the watchdog fired, prepend a structured timeout
    // marker to stderr so the lab harness can detect + surface the
    // timeout. The container died via SIGKILL (exit ~137); without
    // this marker the harness would just see "non-zero exit" with no
    // diagnostic detail about why.
    if timeout_fired.load(Ordering::SeqCst) {
        stderr = format!(
            "darkmux dispatch: INACTIVITY TIMEOUT — no proof-of-work signal in {inactivity_secs}s — \
             container `{container_name}` was killed by the watchdog. The inactivity timer \
             resets on each successful tool call (read / bash / edit / write) and on each \
             compaction event. Genuine thinking-mode hangs and total stalls trigger this; \
             productive dispatches making any tool calls stay alive. Pathological tool patterns \
             are caught by their dedicated detectors (cycle / cascade / cadence-drift) so the \
             deadline can trust activity. Override the default with \
             DARKMUX_INACTIVITY_TIMEOUT_SECONDS=<N>. (#363, #457, #464)\n{stderr}"
        );
    }

    // Signal the tailer to do its final flush + return. join() can
    // theoretically panic if the tailer thread panicked; degrade to a
    // default summary rather than failing the dispatch over a
    // best-effort observability path.
    stop_flag.store(true, Ordering::SeqCst);
    let trajectory_summary = tailer_handle
        .join()
        .unwrap_or_else(|_| TrajectorySummary::default());

    // (#782) Read the runtime's token totals from metrics.json now the
    // container has exited. Best-effort — zero totals on any read failure
    // (this is observability enrichment, never a dispatch-failing path).
    let tokens = read_token_totals(&host_out);

    // 8. Emit dispatch.complete flow record with summary metadata.
    let mut dispatch_complete_payload = serde_json::json!({
        "runtime": "internal",
        "wall_ms": wall_ms,
        "stdout_chars": stdout.chars().count(),
        "stderr_chars": stderr.chars().count(),
        // (#1042) On the error path, carry a bounded stderr TAIL so a failed
        // dispatch is diagnosable from the flow stream alone (the internal
        // path previously emitted only the char count — you couldn't see WHY
        // it failed without shelling into the runner). null on success so
        // clean records aren't bloated. Payload-additive — no FLOW_SCHEMA bump.
        "stderr_excerpt": if exit_code == 0 {
            None
        } else {
            Some(crate::dispatch::tail_excerpt(&stderr, crate::dispatch::STDERR_EXCERPT_MAX))
        },
        "exit_code": exit_code,
        "result_class": if exit_code == 0 { "ok" } else { "error" },
        "total_turns": trajectory_summary.turns,
        "total_tools": trajectory_summary.tool_calls,
        "total_compactions": trajectory_summary.compactions,
        "prompt_tokens": tokens.prompt,
        "completion_tokens": tokens.completion,
        "total_tokens": tokens.total(),
    });
    // (#1187 follow-up) Same field, same reason as `dispatch_start_payload` —
    // parity with `dispatch_remote`'s completion record, and needed by any
    // future by-endpoint consumer (#1186) that reads the terminal record
    // rather than the start record.
    if let Some(label) = &remote_endpoint_raw_label {
        dispatch_complete_payload["endpoint"] = serde_json::json!(label);
    }
    let (action, level) = if exit_code == 0 {
        ("dispatch complete", darkmux_flow::Level::Info)
    } else {
        ("dispatch error", darkmux_flow::Level::Error)
    };
    // (#717, #1230 Packet 0) Emit the terminal record through the bookend
    // guard's `close()` — this both writes the record and disarms the
    // guard, so it never emits a second terminal on the function's normal
    // return (clean complete, or the container-ran-but-failed
    // `dispatch.error` above are both covered the same way).
    bookend.close(crate::dispatch::build_dispatch_record_with_payload(
        level,
        action,
        &opts.role_id,
        &session_id,
        Some(&model),
        mission_id.as_deref(),
        phase_id.as_deref(),
        Some(dispatch_complete_payload),
    ));
    // (#888) The container ran to its terminal record — retain the auto
    // workspace for inspection (status quo). Only error/panic exits BEFORE
    // this point reclaim it.
    workspace_cleanup.disarm();

    // (#557 slice 2) Per-dispatch runtime-turns telemetry. A telemetry
    // sibling of the dispatch.complete record above carrying just the
    // turn count under `category=telemetry, source=runtime` so the
    // observability viewer can render runtime-turns without parsing the
    // Work-category complete payload.
    let _ = darkmux_flow::record(crate::dispatch::build_telemetry_record(
        darkmux_flow::Level::Info,
        "telemetry.runtime",
        "runtime",
        &opts.role_id,
        &session_id,
        Some(&model),
        mission_id.as_deref(),
        phase_id.as_deref(),
        serde_json::json!({ "turns": trajectory_summary.turns }),
    ));

    // (#795) NO at-complete `telemetry.tokens` aggregate here. The tailer
    // emits one `telemetry.tokens` record PER TURN as `model.completed`
    // events land (see `handle_event`), and the savings view SUMS records
    // — re-emitting the dispatch totals at exit would double-count every
    // token. The per-dispatch totals still live on the dispatch.complete
    // payload above for drill-down (the viewer deliberately does not sum
    // that family). #782 originally emitted the aggregate here.

    Ok(DispatchResult {
        exit_code,
        stdout,
        stderr,
        session_id,
        // Host path where the runtime's `.darkmux-runtime/` bookkeeping
        // landed (mounted at `/darkmux-out` in the container). Threaded
        // to coding_task so it reads the trajectory from here rather than
        // from the sandbox.
        out_dir: Some(host_out),
    })
}

/// Summary of what the trajectory tailer surfaced. Used to enrich the
/// dispatch.complete payload with end-of-dispatch counts.
#[derive(Default, Debug, Clone)]
struct TrajectorySummary {
    turns: u32,
    tool_calls: u32,
    compactions: u32,
    heartbeats: u32,
}

/// Token totals the runtime records in `metrics.json` at dispatch exit.
/// Read host-side once the container has exited, then surfaced into the
/// `dispatch.complete` payload so the per-dispatch drill-down shows
/// totals without re-parsing the runtime's stdout envelope (#782). The
/// live "tokens off-meter" aggregation no longer reads from here — it
/// sums the PER-TURN `telemetry.tokens` records the tailer emits (#795).
/// `total` is derived (prompt + completion) at the read site so
/// consumers don't have to.
///
/// `pub` so a second host-side consumer (`mission run`, #782b) reads the
/// same canonical totals from a `DispatchResult.out_dir` rather than
/// duplicating the metrics.json field names.
#[derive(Default, Debug, Clone, Copy)]
pub struct TokenTotals {
    pub prompt: u32,
    pub completion: u32,
}

impl TokenTotals {
    pub fn total(&self) -> u32 {
        self.prompt.saturating_add(self.completion)
    }
}

/// Read `total_prompt_tokens` / `total_completion_tokens` from the
/// runtime's `metrics.json` under `<out_dir>/.darkmux-runtime/`. The
/// runtime writes this file at exit on BOTH the success and error paths
/// (`runtime/src/main.rs`), so it's present once the container has been
/// `wait`ed. Best-effort: a missing/malformed file degrades to zero
/// totals — this is an observability enrichment, never a dispatch
/// failure. Same out-dir the trajectory tailer reads from.
pub fn read_token_totals(out_dir: &Path) -> TokenTotals {
    let metrics_path = out_dir.join(".darkmux-runtime").join("metrics.json");
    let Ok(raw) = fs::read_to_string(&metrics_path) else {
        return TokenTotals::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return TokenTotals::default();
    };
    TokenTotals {
        prompt: v.get("total_prompt_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
        completion: v.get("total_completion_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
    }
}

/// How often the live tailer polls `trajectory.jsonl` while the container
/// is alive. 250ms matches the daemon's `tail_lines` poll cadence in
/// `serve.rs` — short enough for sub-second responsiveness, long enough
/// to keep CPU+IO cost negligible for an idle dispatch.
const TAILER_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// (#557 slice 4) Cadence for the always-on lms + container-CPU
/// telemetry sampler. 2000ms is coarser than the 250ms trajectory tailer
/// on purpose: lms load/unload + container CPU are slow-moving fleet
/// signals (the demo viewer renders CPU as a sparse bar chart, not a live
/// trace), and each tick shells out to `docker stats --no-stream` (a
/// ~1s blocking call) — a tighter cadence would just stack docker calls
/// without adding resolution.
const TELEMETRY_SAMPLE_INTERVAL: Duration = Duration::from_millis(2000);

/// Granularity at which the sampler re-checks its stop flag while waiting
/// out a `TELEMETRY_SAMPLE_INTERVAL`. Mirrors the watchdog's 500ms poll:
/// after `wait_with_output()` sets `sampler_stop`, the thread exits within
/// one poll (≤500ms) instead of blocking for a full 2s sample interval, so
/// the dispatch's `.join()` teardown stays snappy.
const SAMPLER_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Minimum interval between consecutive `dispatch.turn.heartbeat` flow
/// records. The runtime emits one `model.partial` trajectory event per
/// SSE chunk (potentially hundreds per second on a streaming turn);
/// the tailer coalesces them into a coarser heartbeat so the topology
/// viewer's edge-animation 5s decay window stays alive without
/// flooding the flow stream + audit chain. (#231)
const HEARTBEAT_MIN_INTERVAL: Duration = Duration::from_secs(2);

/// Hard cap on `reasoning_text` carried in a `dispatch.reasoning` flow
/// record payload. Thinking-mode models can emit 10MB+ of reasoning on
/// hard problems; without this cap the full text flows through audit +
/// Redis + browser. Truncated payloads carry a marker indicating the
/// original size so the operator knows it was capped. (#231 / S6)
const MAX_REASONING_TEXT_BYTES: usize = 256 * 1024;

/// Hard cap on the OTHER container-written trajectory string fields that flow
/// into a payload — `tool_name`, `finish_reason`, and the assembled detector
/// `detail`. These are short by nature (a tool name, a finish reason, a
/// one-line detector message), so a tight 4 KiB bound is generous for the real
/// thing while stopping an adversarial or buggy container from injecting a
/// pathologically large string into the flow stream / audit chain / Redis /
/// viewer. Defense-in-depth under the viewer's output encoding (#237).
const MAX_TRAJ_FIELD_BYTES: usize = 4 * 1024;

/// Acquire the shared inactivity-deadline lock, recovering from a
/// poisoned mutex instead of panicking. (#890) This deadline is read by
/// the hard-kill watchdog thread every tick and written by the tailer on
/// each proof-of-work event, so its consumers — the watchdog above all —
/// must be the MOST panic-resilient, not the least. A bare
/// `lock().unwrap()` meant that a panic in the tailer (which holds this
/// lock) would poison the mutex, panic the watchdog on its next tick, and
/// silently disable the hard-kill safety net. The guarded value is a
/// plain `Instant`, always valid regardless of which thread panicked, so
/// recovering the poison via `into_inner()` is sound.
fn lock_deadline(deadline: &Mutex<Instant>) -> MutexGuard<'_, Instant> {
    deadline.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run the live trajectory tailer to completion. Polls until `stop_flag`
/// is set, then does one final flush pass to drain any straggler lines
/// the container wrote between the last poll tick and exit. Returns the
/// accumulated event-count summary; never errors (observability path).
///
/// (#457) `inactivity_deadline` is shared with the watchdog thread;
/// the tailer writes a new deadline to it each time a `compaction`
/// trajectory event lands. `inactivity_secs` is the timeout window
/// used to compute the new deadline (`now + inactivity_secs`).
#[allow(clippy::too_many_arguments)]
fn run_tailer(
    // Host path mounted into the container at `/darkmux-out` — where the
    // runtime writes its `.darkmux-runtime/trajectory.jsonl`. SEPARATE
    // from the workspace so the tailer reads the runtime's own
    // bookkeeping, not the tree the agent is editing.
    out_dir: PathBuf,
    session_id: String,
    role_id: String,
    model: String,
    mission_id: Option<String>,
    phase_id: Option<String>,
    stop_flag: Arc<AtomicBool>,
    inactivity_deadline: Arc<Mutex<Instant>>,
    inactivity_secs: u64,
    compaction_threshold: Option<u32>,
) -> TrajectorySummary {
    let trajectory_path = out_dir
        .join(".darkmux-runtime")
        .join("trajectory.jsonl");
    let mut state = TailerState::new(
        trajectory_path,
        session_id,
        role_id,
        model,
        inactivity_deadline,
        inactivity_secs,
    )
    .with_mission(mission_id, phase_id)
    .with_compaction_threshold(compaction_threshold);

    loop {
        state.poll_and_emit();
        if stop_flag.load(Ordering::SeqCst) {
            // Final flush — pick up anything written between the last
            // sleep tick and the container's exit signal.
            state.poll_and_emit();
            break;
        }
        thread::sleep(TAILER_POLL_INTERVAL);
    }

    state.summary
}

/// (#557 slice 4 · #1064) Run the always-on lms + host-load telemetry
/// sampler to completion. Loops until `stop_flag` is set (the main thread
/// sets it after `wait_with_output()` returns), sampling the loaded-model
/// set and the host system load each `TELEMETRY_SAMPLE_INTERVAL` and
/// forwarding each observation into the one flow stream as a
/// `category=telemetry` record.
///
/// **lms (`source=lms`)**: `darkmux_profiles::lms::list_loaded()` is the
/// lms-ps source. Each tick diffs the current loaded set against the
/// previous tick's via `telemetry_sampler::lms_diff`; only CHANGES emit
/// (`{event:"load"|"unload", model, gb?}`).
///
/// **`prev` seeding choice**: the first successful sample emits a BASELINE
/// "load" for every resident model (diffs `cur` against an empty `prev`),
/// then seeds `prev = cur`; subsequent ticks diff normally. The dispatch's
/// model is loaded/selected just BEFORE this sampler thread starts, so the
/// earlier "seed silently, emit nothing" choice meant the run's own model
/// never surfaced and the viewer's model section read "no telemetry yet".
/// Showing the resident set that's serving the dispatch (which is what the
/// `model (lms)` panel is for) beats emitting nothing; any unrelated user
/// models that happen to be resident are minor, informative noise.
/// Implemented via the `seeded` flag.
///
/// **process (`source=process`)**: host **system** load — CPU% (`top`),
/// RAM used% (`vm_stat` + `sysctl -n hw.memsize`), GPU util% (`ioreg`
/// IOAccelerator `"Device Utilization %"`) → `{cpu, mem, gpu}`, all host
/// integer %, each best-effort (a field is omitted for the tick it fails).
/// The container is deliberately NOT sampled: inference runs in LMStudio
/// off-container, so container CPU reads ~0 and answers the wrong question
/// (#814/#1064). Unprivileged — the deeper powermetrics path (sudo) is #2.
///
/// Best-effort throughout — a failed `list_loaded`, a failed `docker
/// stats`, or a failed record never panics or aborts; the worst case is
/// a missing sample. A failed `list_loaded` probe is SKIPPED (the diff
/// runs only against a SUCCESSFUL probe), leaving `prev` untouched for
/// the next tick — so a transient lms hiccup can't emit a flurry of
/// spurious "unload" events.
fn run_telemetry_sampler(
    stop_flag: Arc<AtomicBool>,
    role_id: String,
    session_id: String,
    model: String,
    mission_id: Option<String>,
    phase_id: Option<String>,
) {
    let emit = |source: &str, action: &str, payload: serde_json::Value| {
        let _ = darkmux_flow::record(crate::dispatch::build_telemetry_record(
            darkmux_flow::Level::Info,
            action,
            source,
            &role_id,
            &session_id,
            Some(&model),
            mission_id.as_deref(),
            phase_id.as_deref(),
            payload,
        ));
    };

    let mut prev: Vec<darkmux_types::LoadedModel> = Vec::new();
    let mut seeded = false;

    loop {
        if stop_flag.load(Ordering::SeqCst) {
            break;
        }

        // lms load/unload deltas. Only diff against a SUCCESSFUL probe —
        // a failed `list_loaded` is skipped (leaves `prev` intact) so a
        // transient lms hiccup doesn't emit a flurry of spurious unloads.
        if let Ok(cur) = darkmux_profiles::lms::list_loaded() {
            if !seeded {
                // First successful sample: emit a baseline "load" for every
                // resident model (diff against empty) so the viewer's model
                // section shows what's actually serving THIS dispatch. darkmux
                // loads/selects the model just BEFORE the sampler thread starts,
                // so treating the resident set as "pre-existing, emit nothing"
                // meant the dispatch's own model never appeared — the panel read
                // "no telemetry yet" for a run that clearly had a model loaded.
                for payload in crate::telemetry_sampler::lms_diff(&[], &cur) {
                    emit("lms", "telemetry.lms", payload);
                }
                prev = cur;
                seeded = true;
            } else {
                for payload in crate::telemetry_sampler::lms_diff(&prev, &cur) {
                    emit("lms", "telemetry.lms", payload);
                }
                prev = cur;
            }
        }

        // Host system load — CPU / RAM / GPU utilization%. Inference runs in
        // LMStudio (off-container), so the load worth watching is the HOST's,
        // not the runtime container's (which idles waiting on the model and
        // reads ~0 — the wrong-question problem, #814/#1064). Each read is
        // best-effort and unprivileged; a tick emits whichever of the three
        // succeed (a failed field is simply omitted from the payload).
        // `sample_host` is the shared mechanism (#1247 doctrine surface) —
        // `darkmux-lab`'s review driver samples through the same function.
        let sample = crate::telemetry_sampler::sample_host();
        if sample.cpu.is_some() || sample.mem.is_some() || sample.gpu.is_some() {
            let mut payload = serde_json::Map::new();
            if let Some(c) = sample.cpu {
                payload.insert("cpu".into(), c.into());
            }
            if let Some(m) = sample.mem {
                payload.insert("mem".into(), m.into());
            }
            if let Some(g) = sample.gpu {
                payload.insert("gpu".into(), g.into());
            }
            emit(
                "process",
                "telemetry.process",
                serde_json::Value::Object(payload),
            );
        }

        // Wait out the sample interval, but poll the stop flag every
        // `SAMPLER_POLL_INTERVAL` so teardown after `wait_with_output()`
        // is prompt (≤500ms) rather than blocking for a full 2s tick.
        let mut slept = Duration::ZERO;
        while slept < TELEMETRY_SAMPLE_INTERVAL {
            if stop_flag.load(Ordering::SeqCst) {
                return;
            }
            let nap = SAMPLER_POLL_INTERVAL.min(TELEMETRY_SAMPLE_INTERVAL - slept);
            thread::sleep(nap);
            slept += nap;
        }
    }
}

/// State machine for tailing `trajectory.jsonl`. Tracks the file offset,
/// Drain complete (newline-terminated) lines from a byte buffer,
/// returning each as a decoded `String`. The buffer is mutated in
/// place: drained line bytes are removed; any partial tail without a
/// trailing `\n` stays in the buffer for the next call.
///
/// Decoding via `from_utf8_lossy` happens ONCE per complete line —
/// never on a partial read. This is the load-bearing invariant for
/// #329: multi-byte UTF-8 sequences (emoji, CJK) that straddle a
/// read boundary stay intact across calls. Pre-fix, the buffer was a
/// `String` with per-poll `from_utf8_lossy(&buf)`, which replaced
/// any partial multi-byte tail with U+FFFD and silently corrupted
/// the subsequent JSON payload.
///
/// Empty lines are skipped (matches the pre-fix loop's semantics).
fn drain_complete_lines_from_bytes(pending: &mut Vec<u8>) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
        // Drain through and including the newline; line content is
        // everything before the newline byte.
        let line_bytes: Vec<u8> = pending.drain(..=nl).collect();
        let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]).into_owned();
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

/// any partial-line tail bytes carried across polls, and the last
/// heartbeat instant for rate limiting.
struct TailerState {
    trajectory_path: PathBuf,
    offset: u64,
    /// Trailing partial line carried from one poll to the next when the
    /// file ends mid-line (a write was in progress at our read).
    ///
    /// Raw bytes — NOT a string. Multi-byte UTF-8 characters (emoji,
    /// CJK) can split across `poll_and_emit` rounds; decoding via
    /// `from_utf8_lossy` on partial bytes would emit U+FFFD
    /// replacement chars and silently corrupt the JSON payload (#329).
    /// We accumulate bytes here and decode per-line, after the
    /// trailing newline arrives.
    pending: Vec<u8>,
    session_id: String,
    role_id: String,
    model: String,
    /// (#714) Mission/phase this dispatch belongs to (when phase-bound),
    /// stamped onto every per-event flow record so they group under the
    /// mission in the observability view. `None` for a one-off dispatch.
    mission_id: Option<String>,
    phase_id: Option<String>,
    last_heartbeat_at: Option<Instant>,
    summary: TrajectorySummary,
    /// (#457) Shared with the watchdog thread. Tailer writes a new
    /// deadline (`now + inactivity_secs`) when a `compaction` event
    /// fires; watchdog reads each tick to decide whether to kill the
    /// container. `None` in test fixtures that don't exercise the
    /// reset path.
    inactivity_deadline: Option<Arc<Mutex<Instant>>>,
    inactivity_secs: u64,
    /// (#threshold) Effective compaction threshold — the prompt-token level the
    /// runtime triggers compaction at — forwarded on every context telemetry
    /// record so the viewer can draw the trigger line. `None` in tests / when
    /// no window is configured.
    compaction_threshold: Option<u32>,
}

impl TailerState {
    fn new(
        trajectory_path: PathBuf,
        session_id: String,
        role_id: String,
        model: String,
        inactivity_deadline: Arc<Mutex<Instant>>,
        inactivity_secs: u64,
    ) -> Self {
        Self {
            trajectory_path,
            offset: 0,
            pending: Vec::new(),
            session_id,
            role_id,
            model,
            mission_id: None,
            phase_id: None,
            last_heartbeat_at: None,
            summary: TrajectorySummary::default(),
            inactivity_deadline: Some(inactivity_deadline),
            inactivity_secs,
            compaction_threshold: None,
        }
    }

    /// (#714) Stamp the phase → mission this dispatch belongs to so the
    /// per-event flow records group under the mission. Builder-style so the
    /// `new`/`new_for_test` call sites (incl. unit tests) stay unchanged;
    /// only the production `run_tailer` opts in.
    fn with_mission(mut self, mission_id: Option<String>, phase_id: Option<String>) -> Self {
        self.mission_id = mission_id;
        self.phase_id = phase_id;
        self
    }

    /// (#threshold) Forward the effective compaction threshold so context
    /// telemetry carries it and the viewer can draw the trigger line.
    /// Builder-style; only production `run_tailer` opts in (new/test callers
    /// keep the `None` default).
    fn with_compaction_threshold(mut self, threshold: Option<u32>) -> Self {
        self.compaction_threshold = threshold;
        self
    }

    /// Test-only constructor — no inactivity-deadline plumbing. Used by
    /// the unit tests that exercise event-handling shape without
    /// spawning a watchdog thread.
    #[cfg(test)]
    fn new_for_test(
        trajectory_path: PathBuf,
        session_id: String,
        role_id: String,
        model: String,
    ) -> Self {
        Self {
            trajectory_path,
            offset: 0,
            pending: Vec::new(),
            session_id,
            role_id,
            model,
            mission_id: None,
            phase_id: None,
            last_heartbeat_at: None,
            summary: TrajectorySummary::default(),
            inactivity_deadline: None,
            inactivity_secs: 600,
            compaction_threshold: None,
        }
    }

    /// One poll round: open the trajectory file, read new bytes since
    /// the previous offset, drain complete lines, dispatch each event.
    /// Silent on errors — file may not exist yet (container hasn't
    /// written) and any IO hiccup is best-effort.
    fn poll_and_emit(&mut self) {
        use std::io::{Read, Seek, SeekFrom};

        let mut file = match std::fs::File::open(&self.trajectory_path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let size = match file.metadata() {
            Ok(m) => m.len(),
            Err(_) => return,
        };
        // File truncated below our offset (shouldn't happen in practice
        // since the runtime writes append-only, but defensive): reset.
        if size < self.offset {
            self.offset = 0;
            self.pending.clear();
        }
        if size <= self.offset {
            return;
        }

        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut buf = Vec::with_capacity((size - self.offset) as usize);
        if file.read_to_end(&mut buf).is_err() {
            return;
        }
        self.offset = size;

        // Append raw bytes; decode happens per-line (after the
        // trailing newline arrives) so multi-byte UTF-8 chars that
        // straddle a poll boundary don't corrupt to U+FFFD (#329).
        self.pending.extend_from_slice(&buf);
        for line in drain_complete_lines_from_bytes(&mut self.pending) {
            self.handle_event(&line);
        }
    }

    fn handle_event(&mut self, line: &str) {
        let event: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return,
        };
        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match event_type {
            "model.completed" => {
                self.summary.turns += 1;
                let payload = serde_json::json!({
                    "turn_seq": event.get("seq"),
                    "finish_reason": cap_json_str(event.get("finish_reason"), MAX_TRAJ_FIELD_BYTES),
                    "tool_calls_count": event.get("tool_calls").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
                    "usage": event.get("usage"),
                });
                self.emit("dispatch.turn", darkmux_flow::Level::Info, payload);
                // (#795) Per-turn token telemetry — the live "tokens
                // off-meter" odometer climbs DURING the dispatch, not just
                // at complete. Each turn's billed usage is its own
                // `telemetry.tokens` record; the viewer SUMS records, and
                // per-turn billed prompt tokens genuinely sum to the
                // runtime's metrics totals (each turn re-sends context —
                // same accumulator the runtime uses). The former
                // at-complete aggregate is gone so nothing double-counts.
                // Skipped when the event carries no `usage` (such turns
                // also don't accumulate in metrics.json — symmetric).
                if let Some(tokens_payload) = turn_tokens_payload(&event) {
                    self.emit_telemetry("tokens", "telemetry.tokens", tokens_payload);
                }
            }
            "tool.completed" => {
                self.summary.tool_calls += 1;
                // (#464) Tool completion is the second proof-of-work signal
                // (alongside compaction). Reset the inactivity deadline:
                // the model is actively producing or inspecting state,
                // which means the dispatch is alive in a way the deadline
                // should respect. Pathological tool patterns (cycle on
                // same args, cascade of failures, edit-drift on same file,
                // reasoning loops) are caught by their dedicated detectors
                // — per-mole-hole guards make this generous reset safe.
                //
                // Operator-stated principle (Beat 54): "at least one guard
                // per mole hole." Read alone isn't proof-of-work in
                // isolation, but read patterns that look like spinning
                // are caught by the cycle detector. Edit drift is caught
                // by the cadence-drift detector (post-#465 redesign).
                // The deadline trusts activity; the detectors catch
                // struggle.
                //
                // (#469) ONLY a successful tool call resets the deadline.
                // A model fast-failing with varying tool calls (different
                // args each time, so the cycle detector misses; failures
                // interleaved with reads, so the failure-rate detector's
                // consecutive-count never trips) would otherwise keep the
                // deadline alive indefinitely. The `ok` field closes that
                // loophole. Backward-compat: events predating the field
                // (no `ok`) are treated as success so old trajectories
                // behave as before.
                let tool_ok = event
                    .get("ok")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                if tool_ok {
                    if let Some(deadline) = &self.inactivity_deadline {
                        let new_deadline =
                            Instant::now() + Duration::from_secs(self.inactivity_secs);
                        *lock_deadline(deadline) = new_deadline;
                    }
                }
                let payload = serde_json::json!({
                    "tool_seq": event.get("tool_seq"),
                    "tool_name": cap_json_str(event.get("tool_name"), MAX_TRAJ_FIELD_BYTES),
                    // The actual arguments (search pattern / path / command) so the
                    // viewer can show WHAT the model did, not just that it acted.
                    // Already capped in the runtime (MAX_TOOL_ARGS_CHARS); re-bound
                    // here defensively against MAX_TRAJ_FIELD_BYTES for the flow record.
                    "args": cap_json_str(event.get("args"), MAX_TRAJ_FIELD_BYTES),
                    "args_chars": event.get("args_chars"),
                    "result_chars": event.get("result_chars"),
                    "ok": tool_ok,
                });
                self.emit("dispatch.tool", darkmux_flow::Level::Info, payload);
            }
            "compaction" => {
                self.summary.compactions += 1;
                // (#457) Reset the inactivity deadline shared with the
                // watchdog thread. A successful compaction is observable
                // proof the dispatch is alive (compactor model ran,
                // primary accepted new state, turns continued). Without
                // this reset, productive dispatches that legitimately
                // need many minutes between compactions get killed by
                // the absolute-deadline shape we replaced.
                if let Some(deadline) = &self.inactivity_deadline {
                    let new_deadline =
                        Instant::now() + Duration::from_secs(self.inactivity_secs);
                    *lock_deadline(deadline) = new_deadline;
                }
                let payload = serde_json::json!({
                    "generation": event.get("generation"),
                    "before_messages": event.get("before_messages"),
                    "after_messages": event.get("after_messages"),
                    "summary_chars": event.get("summary_chars"),
                });
                self.emit("dispatch.compaction", darkmux_flow::Level::Info, payload);
                // (#557 slice 3) Compaction token telemetry — the drop in
                // the context-occupancy sawtooth. The runtime now carries
                // `tokens_before` (EXACT prompt-token count that triggered
                // compaction) + `tokens_after` (chars/4 estimate of the
                // compacted buffer) on the compaction event; forward them
                // as a `source=compaction` telemetry record the viewer
                // reads as `{from, to}`.
                self.emit_telemetry("compaction", "telemetry.compaction", serde_json::json!({
                    "from": event.get("tokens_before"),
                    "to": event.get("tokens_after"),
                }));
            }
            "model.reasoning" => {
                // The runtime emits these when it parses <think>...</think>
                // blocks from the assistant content (#204). The full
                // reasoning text rides in payload so the flow viewer can
                // render a collapse/expand block. Capped at
                // MAX_REASONING_TEXT_BYTES so a single huge thinking
                // session can't blow up downstream storage. (#231 / S6)
                let reasoning_text = cap_reasoning_text(event.get("reasoning_text"));
                let payload = serde_json::json!({
                    "turn_seq": event.get("seq"),
                    "reasoning_chars": event.get("reasoning_chars"),
                    "reasoning_text": reasoning_text,
                    "reasoning_format": event.get("reasoning_format").unwrap_or(&serde_json::Value::String("inline-think-tags".into())),
                });
                self.emit("dispatch.reasoning", darkmux_flow::Level::Info, payload);
            }
            "dispatch.feedback.injected" => {
                // (#454 feedback-injection scaffold) Forward the new
                // runtime trajectory event into the flow stream so
                // fleet observability surfaces (flow tail, topology
                // viewer, audit chain) can see when the runtime's
                // meta-awareness channel delivered a system message
                // to the model. Companion to the per-signal flow
                // forwarders (cycle/cascade trajectory events today
                // do NOT have flow forwarders, but feedback.injected
                // is the model-visible delivery event that operators
                // monitoring fleet behavior most want surfaced).
                let payload = serde_json::json!({
                    "turn_seq": event.get("seq"),
                    "message_count": event.get("message_count"),
                    "signal_kinds": event.get("signal_kinds"),
                });
                self.emit("dispatch.feedback.injected", darkmux_flow::Level::Info, payload);
            }
            "model.partial" => {
                // Per-SSE-chunk events coalesced into a coarser heartbeat
                // (rate-limited via HEARTBEAT_MIN_INTERVAL). Keeps
                // topology edges animated during long streaming turns
                // without flooding the flow stream + audit chain. (#231)
                let now = Instant::now();
                let should_emit = match self.last_heartbeat_at {
                    None => true,
                    Some(prev) => now.duration_since(prev) >= HEARTBEAT_MIN_INTERVAL,
                };
                if should_emit {
                    self.last_heartbeat_at = Some(now);
                    self.summary.heartbeats += 1;
                    // (#1222 shakedown-3) Streamed chunks are the third
                    // proof-of-work signal. A model.partial event only fires
                    // when the model actually delivered tokens — transport-
                    // level liveness, exactly what the watchdog exists to
                    // check (a wedged server/network/container delivers no
                    // chunks, so true hangs still die). Two dispatches were
                    // killed mid-generation at 8+ minutes of LEGITIMATE
                    // reasoning under a raised per-call cap because only
                    // tool.completed/compaction reset the deadline.
                    // Boundedness is not this guard's job (per-mole-hole,
                    // #464): the per-call cap ends every turn, the stall
                    // budget bounds repeated runaways, the detectors catch
                    // patterns, max_turns/max_tokens bound totals. Reset
                    // rides the heartbeat rate-limit gate, so it costs one
                    // mutex write per HEARTBEAT_MIN_INTERVAL, not per chunk.
                    if let Some(deadline) = &self.inactivity_deadline {
                        *lock_deadline(deadline) =
                            Instant::now() + Duration::from_secs(self.inactivity_secs);
                    }
                    let payload = serde_json::json!({
                        "runtime": "internal",
                        "turn_seq": event.get("seq"),
                        "partial_index": event.get("partial_index"),
                        "cumulative_chars": event.get("cumulative_chars"),
                    });
                    self.emit("dispatch.turn.heartbeat", darkmux_flow::Level::Info, payload);
                }
            }
            // (#557 slice 2) Detector trajectory events → telemetry flow
            // records. Each detector firing becomes one `category=telemetry,
            // source=detector` record carrying a {kind, severity, detail}
            // payload the observability viewer renders. The runtime already
            // records these in its own trajectory; forwarding them into the
            // one flow stream is what makes detector firings visible in the
            // unified viewer (the keystone of #557). The kind/severity/detail
            // mapping lives in the pure `detector_telemetry_payload` helper
            // so it can be unit-tested without the process-global flow sink.
            "dispatch.cycle.suspected"
            | "dispatch.reasoning_loop.suspected"
            | "dispatch.tool.repeated_failure"
            | "dispatch.intra_turn_stall.recovered"
            | "dispatch.per_turn_cap.salvaged" => {
                if let Some(payload) = detector_telemetry_payload(event_type, &event) {
                    self.emit_telemetry("detector", "telemetry.detector", payload);
                }
            }
            // (#557 slice 3) Per-turn context-window occupancy sawtooth.
            // The runtime emits one `dispatch.context` trajectory event per
            // turn carrying the EXACT prompt-token count (`used`) + the
            // configured n_ctx (`max`, null when unconfigured). Forward it
            // into the one flow stream as a `source=context` telemetry
            // record the observability viewer reads as `{used, max}`.
            "dispatch.context" => {
                self.emit_telemetry("context", "telemetry.context", serde_json::json!({
                    "used": event.get("used"),
                    "max": event.get("max"),
                    "threshold": self.compaction_threshold,
                }));
            }
            _ => {
                // Other event types (dispatch.start/complete from the
                // runtime side; model.streaming.start/end) are ignored —
                // the CLI emits canonical dispatch bookends; streaming
                // start/end events are runtime-internal observability
                // with no flow-stream consumer yet.
            }
        }
    }

    fn emit(&self, action: &str, level: darkmux_flow::Level, payload: serde_json::Value) {
        let _ = darkmux_flow::record(crate::dispatch::build_dispatch_record_with_payload(
            level,
            action,
            &self.role_id,
            &self.session_id,
            Some(&self.model),
            self.mission_id.as_deref(),
            self.phase_id.as_deref(),
            Some(payload),
        ));
    }

    /// (#557 slice 2) Emit a telemetry flow record. Same plumbing as
    /// `emit` but routes through `build_telemetry_record` so the record
    /// lands under `category=telemetry` with a caller-supplied `source`
    /// (`"detector"`, `"runtime"`, …) the observability viewer keys on.
    fn emit_telemetry(&self, source: &str, action: &str, payload: serde_json::Value) {
        let _ = darkmux_flow::record(crate::dispatch::build_telemetry_record(
            darkmux_flow::Level::Info,
            action,
            source,
            &self.role_id,
            &self.session_id,
            Some(&self.model),
            self.mission_id.as_deref(),
            self.phase_id.as_deref(),
            payload,
        ));
    }
}

/// (#795) Map a `model.completed` trajectory event to the per-turn
/// `telemetry.tokens` payload — `{turn_seq, prompt_tokens,
/// completion_tokens, total_tokens}`. Pure (no IO, no global sink) so the
/// mapping is unit-testable in isolation from `handle_event`'s
/// flow-record emission, same pattern as `detector_telemetry_payload`.
///
/// Returns `None` when the event carries no `usage` object (upstream
/// omitted it — rare). Such turns also don't accumulate into the
/// runtime's metrics.json totals (`loop_runner.rs` only adds when
/// `response.usage` is `Some`), so skipping the record preserves the
/// invariant that per-turn `telemetry.tokens` records SUM to the
/// dispatch's metrics totals exactly. Absent token counts inside a
/// present `usage` object degrade to 0 (defensive; the runtime always
/// writes both fields).
fn turn_tokens_payload(event: &serde_json::Value) -> Option<serde_json::Value> {
    let usage = event.get("usage").filter(|u| u.is_object())?;
    let prompt = usage.get("prompt_tokens").and_then(|n| n.as_u64()).unwrap_or(0);
    let completion = usage.get("completion_tokens").and_then(|n| n.as_u64()).unwrap_or(0);
    Some(serde_json::json!({
        "turn_seq": event.get("seq"),
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt.saturating_add(completion),
    }))
}

/// (#557 slice 2) Map a detector trajectory event to its telemetry
/// payload — the `{kind, severity, detail}` object the observability
/// viewer renders. Pure (no IO, no global sink) so the mapping is
/// unit-testable in isolation from `handle_event`'s flow-record emission.
///
/// `event_type` is the already-extracted `event["type"]` string; `event`
/// is the parsed trajectory line. Returns `None` for event types this
/// helper doesn't map (the caller only invokes it for the five detector
/// types, so `None` is defensive — it never fires on the happy path).
///
/// Field accessors are all safe (`unwrap_or` defaults) so a malformed or
/// partial detector event still produces a renderable record rather than
/// dropping the firing. `completion_tokens` on the intra-turn-stall event
/// may be JSON null (upstream omitted `usage`); that renders as "unknown"
/// rather than a misleading "0".
fn detector_telemetry_payload(
    event_type: &str,
    event: &serde_json::Value,
) -> Option<serde_json::Value> {
    let str_field = |k: &str| event.get(k).and_then(|v| v.as_str()).unwrap_or("?");
    let u64_field = |k: &str| event.get(k).and_then(|v| v.as_u64()).unwrap_or(0);

    let (kind, severity, detail) = match event_type {
        "dispatch.cycle.suspected" => {
            let tool_name = str_field("tool_name");
            let count = u64_field("count");
            let window_size = u64_field("window_size");
            (
                "cycle",
                "warn",
                format!(
                    "`{tool_name}` called {count}× in the last {window_size} tool calls — repeated-tool-call cycle (#418)"
                ),
            )
        }
        "dispatch.reasoning_loop.suspected" => {
            let count = u64_field("count");
            let window_size = u64_field("window_size");
            (
                "reasoning-loop",
                "warn",
                format!(
                    "same reasoning repeated {count}× in {window_size} turns — reasoning loop (#461)"
                ),
            )
        }
        "dispatch.tool.repeated_failure" => {
            let tool_name = str_field("tool_name");
            let failure_count = u64_field("failure_count");
            (
                "tool-failure",
                "warn",
                format!("{failure_count} failures of `{tool_name}` since it last succeeded (#419)"),
            )
        }
        "dispatch.intra_turn_stall.recovered" => {
            // completion_tokens may be JSON null (upstream omitted `usage`);
            // render "unknown" rather than a misleading 0 that reads
            // identical to a real small count.
            let completion_tokens = event
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let recoveries_used = u64_field("recoveries_used");
            let recoveries_budget = u64_field("recoveries_budget");
            (
                "intra-turn-stall",
                "info",
                format!(
                    "runaway-reasoning turn dropped + recovered (budget {recoveries_used}/{recoveries_budget}, {completion_tokens} tokens) (#414)"
                ),
            )
        }
        "dispatch.per_turn_cap.salvaged" => {
            let completion_tokens = u64_field("completion_tokens");
            let cap = u64_field("cap");
            let salvaged_tool_calls = u64_field("salvaged_tool_calls");
            (
                "per-turn-cap",
                "info",
                format!(
                    "{salvaged_tool_calls} tool call(s) salvaged at the per-call cap ({completion_tokens}/{cap} tokens) (#479)"
                ),
            )
        }
        _ => return None,
    };

    // (#237) The detail embeds a container-written `tool_name`; bound the
    // assembled string so a pathologically large tool name can't bloat the
    // telemetry record. kind/severity are fixed literals (not container-set).
    let detail = cap_str(&detail, MAX_TRAJ_FIELD_BYTES);

    let mut payload = serde_json::json!({
        "kind": kind,
        "severity": severity,
        "detail": detail,
    });

    // (#994 engagement-context capture) Key the firing to the file it happened
    // in, so this *caution* can later be retrieved for the same file and fed
    // into the next dispatch's brief. We derive `area.files` from the
    // detector's own args. Slice 1 covered `dispatch.cycle.suspected`; #1001
    // extended it to `dispatch.tool.repeated_failure` (now carries
    // `canonical_args`) and added the firing-time `code_hash` (computed inside
    // the container) for staleness ranking. (`symbols` remains a later
    // refinement — the load-bearing staleness signal is the hash.)
    if let Some(area) = detector_area(event_type, event) {
        payload["area"] = area;
    }

    Some(payload)
}

/// (#994 engagement-context capture, slice 1) Derive the `area` of the
/// engagement a detector firing touched — the file the cycled tool call
/// targeted — for stamping into the telemetry record's payload (`area.files`).
///
/// Today only `dispatch.cycle.suspected` carries the tool-call args
/// (`canonical_args`, already a normalized JSON object for known tools — see
/// `runtime/src/cycle_detector.rs::canonical_args`), so it is the only detector
/// with a host-derivable file here, and only for the genuinely file-editing
/// tools: `read`/`edit`/`write` carry a target-file `path`. A `search` cycle's
/// `path` is the search *root directory* (not a target file) and a `bash`
/// cycle carries a `command`, so neither keys a file — those, like the
/// turn-level detectors (reasoning-loop / intra-turn-stall / per-turn-cap) and
/// `dispatch.tool.repeated_failure` (no args today), return `None` → the caller
/// omits `area` rather than recording a fileless or directory-as-file caution
/// (those become a runtime-side slice 2).
///
/// Returns `None` when no file path is derivable.
fn detector_area(event_type: &str, event: &serde_json::Value) -> Option<serde_json::Value> {
    let path = match event_type {
        // (#1001) Both the cycle detector and the tool-failure-cascade detector
        // carry the tool's `canonical_args`. Allowlist the file-editing tools:
        // their canonical `path` is a target file. `search`'s `path` is a
        // directory and `bash` carries a `command`, so a firing in either is
        // engagement-level, not file-scoped. Unknown tools (raw, unfiltered
        // canonical args) fall through too. Clean-by-construction beats storing
        // a directory in a field named `files`.
        "dispatch.cycle.suspected" | "dispatch.tool.repeated_failure" => {
            let tool = event.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
            if !matches!(tool, "read" | "edit" | "write") {
                return None;
            }
            let raw = event.get("canonical_args").and_then(|v| v.as_str())?;
            extract_tool_target_path(raw)?
        }
        _ => return None,
    };
    // The path is container-written (the model chose it), so bound it the same
    // way the detector `detail` is bounded (#237) — a pathologically long path
    // can't bloat the telemetry record.
    let path = cap_str(&path, MAX_TRAJ_FIELD_BYTES);
    let mut area = serde_json::json!({ "files": [path] });
    // (#1001) Forward the runtime's firing-time content hash when present, so
    // retrieval can rank this caution down once the file has changed
    // (staleness). Bounded like the path; absent for a non-file tool.
    if let Some(h) = event.get("code_hash").and_then(|v| v.as_str()) {
        area["code_hash"] = serde_json::Value::String(cap_str(h, MAX_TRAJ_FIELD_BYTES));
    }
    Some(area)
}

/// (#994) Parse the target file path from a tool call's canonicalized JSON
/// args — the `path` field the runtime's `read`/`edit`/`write` tools carry.
/// This mirrors only the *parse* step of the runtime-side
/// `extract_edit_target_path` (`runtime/src/loop_runner.rs`); the runtime crate
/// is out-of-workspace, so the tiny parser is replicated here per the repo's
/// "inline-over-crate for small needs" convention rather than shared across the
/// boundary. It deliberately does NOT replicate that function's lexical path
/// normalization (#471): the #994 retrieve slice normalizes both the stored
/// keys and the current files *together* at match time, so capturing the path
/// verbatim here is sufficient (and keeps the raw record faithful to what the
/// model passed). The normalizer lands once, in the retrieve slice where it is
/// actually used, rather than speculatively here.
///
/// Returns `None` when the args don't parse as JSON or carry no string `path`.
fn extract_tool_target_path(raw_args: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(raw_args)
        .ok()
        .and_then(|v| v.get("path").and_then(|p| p.as_str()).map(String::from))
}

/// Cap a string at `max` bytes, truncating at a UTF-8 char boundary (so the
/// result stays valid) and appending a marker that records the original size.
/// Short strings are returned unchanged. The single primitive behind both
/// `cap_json_str` and the detector-`detail` bound (#237).
fn cap_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}… [truncated; original {} chars / {} bytes]",
        &s[..cut],
        s.chars().count(),
        s.len()
    )
}

/// Cap a JSON string value at `max` bytes (see `cap_str`). Non-string values
/// pass through unchanged; `None` becomes JSON null. Container-written
/// trajectory fields flow into flow-record payloads (→ flow stream, audit
/// chain, Redis, viewer); bounding them at ingest stops an adversarial or buggy
/// container from injecting a pathologically large string (#237 defense-in-
/// depth, layered under the viewer's output encoding).
fn cap_json_str(value: Option<&serde_json::Value>, max: usize) -> serde_json::Value {
    let Some(v) = value else {
        return serde_json::Value::Null;
    };
    let Some(s) = v.as_str() else {
        return v.clone();
    };
    if s.len() <= max {
        return v.clone();
    }
    serde_json::Value::String(cap_str(s, max))
}

/// `reasoning_text` cap — the large-text case (thinking-mode output). Delegates
/// to the shared `cap_json_str` with the reasoning-specific bound. (#231 / S6)
fn cap_reasoning_text(value: Option<&serde_json::Value>) -> serde_json::Value {
    cap_json_str(value, MAX_REASONING_TEXT_BYTES)
}

/// Result of probing the host for the internal Docker runtime's two
/// prerequisites (daemon reachable + `darkmux-runtime:latest` built).
///
/// Single source of truth shared by two consumers with different
/// presentation: the dispatch-time `check_docker_preflight()` maps these
/// to a hard bail (a stuck dispatch is worse than a refused one), while
/// `darkmux doctor`'s `check_docker_runtime` maps them to a non-fatal Warn
/// so a fresh operator learns the requirement at setup time rather than
/// only when their first dispatch fails (#680).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerRuntimeStatus {
    /// Docker daemon reachable AND the `RUNTIME_IMAGE` is built — ready.
    Ready,
    /// The `docker` binary isn't on PATH.
    BinaryMissing,
    /// `docker` is present but `docker version` failed (daemon not running).
    /// Carries the trimmed stderr for diagnostics.
    DaemonUnreachable(String),
    /// Docker is up but `RUNTIME_IMAGE` isn't built locally.
    ImageMissing,
    /// Couldn't run the image probe even though the daemon answered (rare —
    /// same binary as the version probe). Carries the error string.
    ProbeError(String),
}

/// Probe the host for Docker availability + the runtime image. Pure of any
/// presentation decision — callers map the returned status to a bail
/// (dispatch) or a Warn (doctor).
pub fn docker_runtime_status() -> DockerRuntimeStatus {
    // Step 1: docker binary exists + daemon is reachable.
    match Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
    {
        Ok(out) if out.status.success() => {} // Docker daemon up — fall through
        Ok(out) => {
            return DockerRuntimeStatus::DaemonUnreachable(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            );
        }
        Err(_) => return DockerRuntimeStatus::BinaryMissing,
    }

    // Step 2: a darkmux runtime image is present locally — either the dev-built
    // `RUNTIME_IMAGE` tag OR the version-pinned GHCR image pulled on demand
    // (#759). `Ready` means at least one is present (no pull needed); otherwise
    // `ImageMissing`, which the dispatch preflight resolves by pulling GHCR.
    // (Daemon-unreachable cases were caught in Step 1.)
    if image_present_locally(RUNTIME_IMAGE) || image_present_locally(&ghcr_runtime_image()) {
        DockerRuntimeStatus::Ready
    } else {
        DockerRuntimeStatus::ImageMissing
    }
}

/// Verify Docker is reachable, then resolve the darkmux runtime image —
/// pulling the version-pinned GHCR image on demand if no local image exists
/// (#759). Returns the resolved image ref. Called by `dispatch()` BEFORE the
/// role-load / model-probe / workspace setup so a new user without Docker gets
/// a clean, operator-actionable bail, and a `brew install` user without a
/// local image gets a one-time pull instead of a "build from source" dead-end.
fn check_docker_preflight() -> Result<String> {
    match docker_runtime_status() {
        // Daemon up (image present OR absent) — resolve the darkmux image,
        // pulling the version-pinned GHCR image on demand if none is local.
        DockerRuntimeStatus::Ready | DockerRuntimeStatus::ImageMissing => {
            ensure_darkmux_image_present()
        }
        // Docker missing / daemon unreachable / probe error → actionable bail
        // (reuse the pure mapper, which returns Err for all of these). The
        // `.map` only adapts the Ok type so the `Result<String>` signature
        // lines up — these variants never produce Ok, so the `String` is never
        // built.
        other => preflight_result_for(other).map(|()| String::new()),
    }
}

/// Map a probe result to the dispatch-time bail (pure — unit-testable
/// without Docker on the host).
fn preflight_result_for(status: DockerRuntimeStatus) -> Result<()> {
    match status {
        DockerRuntimeStatus::Ready => Ok(()),
        DockerRuntimeStatus::DaemonUnreachable(stderr) => bail!(
            "darkmux's runtime requires Docker, but `docker version` failed:\n  {}\n\
             Start Docker Desktop and retry.",
            stderr
        ),
        DockerRuntimeStatus::BinaryMissing => bail!(
            "darkmux's runtime requires Docker, but the `docker` binary isn't on PATH.\n\
             Install Docker Desktop (https://www.docker.com/products/docker-desktop) and retry."
        ),
        DockerRuntimeStatus::ImageMissing => bail!(
            "no darkmux runtime image found locally. darkmux pulls the \
             version-pinned image `{}` from GHCR on demand; if that pull \
             can't run, build it once from a darkmux source checkout:\n  \
             docker build -t {RUNTIME_IMAGE} runtime/",
            ghcr_runtime_image()
        ),
        DockerRuntimeStatus::ProbeError(e) => {
            Err(anyhow!("running `docker images` to check for runtime image: {e}"))
        }
    }
}

/// (#450, E14 refactor 1b) Resolve the model id this internal-runtime
/// dispatch should target for the given role.
///
/// Selection chain:
/// 1. Load the profile registry, then resolve the active profile via
///    `ProfileRegistry::resolve_active(profile_override)` (#1054): the CLI
///    `--profile` override when it names a profile defined here, else
///    `registry.default_profile`. An override that ISN'T defined on this
///    machine falls back to `default_profile` (logged) rather than failing —
///    a machine-agnostic caller names the profile it wants and each machine
///    maps it to a lab-validated model. When nothing resolves, fall back to
///    `probe_loaded_model()`.
/// 2. Look up the profile + call `select_model(role, profile, skill_lookup)`,
///    which capability-scores the role against the profile's models (#590),
///    falling back to the profile's default model when no vectors
///    are populated (ModelRole removed in #601).
/// 3. On no-default / no-model failures (a registry that LOADED fine but has
///    nothing usable), log a deprecation warning + fall back to
///    `probe_loaded_model()`. Back-compat for pre-refactor-1b
///    configurations; the warning points the operator at the migration.
///
/// The fallback is intentional but loud for those two cases. Per memory
/// note `feedback_model_unload_load_authority`, silent reliance on
/// "whatever LMStudio happens to have loaded" is the contaminating-dispatch
/// anti-pattern. The deprecation warning makes the misconfiguration
/// operator-visible while keeping pre-refactor-1b setups working.
///
/// (#1269) A registry-LOAD failure (step 1 itself erroring — malformed
/// JSON, a bad profile) is a DIFFERENT failure class and does NOT fall
/// through to `probe_loaded_model()`: routing a broken config file into an
/// unrelated LMStudio probe just produces a second, more confusing error on
/// top of the first. One config mistake gets ONE clear, named error.
///
/// `skip_lmstudio_residency`: mock-model harness escape hatch (real Docker
/// dispatch, fake "model" — see `crates/darkmux-crew/tests/mock_dispatch_proof.rs`
/// and `tools/darkmux-mock-model`). When `true` (set by `dispatch()` exactly
/// when `opts.model_base_url_override` is `Some`), this function resolves
/// the model NAME only — it skips `ensure_model_loaded_at_ctx` (a REAL `lms
/// load` call) and the loaded-vs-selected cross-check (a REAL `lms ps`
/// call). Without this, a dispatch pointed at a scripted mock server still
/// tried to load the mock's made-up model id into the operator's REAL
/// LMStudio — `lms load <unknown-id>` fell into an interactive
/// disambiguation picker and hung forever waiting on a TTY that a
/// non-interactive test never provides (found empirically building the
/// mock-model harness: `resolve_dispatch_model_internal` looked I/O-free
/// from its name and doc, but touches real LMStudio as a side effect of
/// "just resolving a model id"). `false` everywhere else preserves
/// existing behavior exactly.
fn resolve_dispatch_model_internal(
    role: &crate::types::Role,
    profile_override: Option<&str>,
    config_path: Option<&str>,
    skip_lmstudio_residency: bool,
) -> Result<String> {
    use crate::select::select_model;
    use darkmux_profiles::profiles::load_registry;

    let loaded = load_registry(config_path).map_err(|e| {
        anyhow!(
            "darkmux dispatch: profile registry not loadable ({e}). \
             Fix the registry file named above — this is a hard stop, not a \
             fallback to whatever LMStudio has loaded, since a broken config \
             can't tell us what you intended to dispatch to. (#1269)"
        )
    })?;

    // (#1282) A REQUESTED profile that was quarantined at load is a hard
    // stop with the entry's own parse error — letting it reach the #1054
    // "not defined" fallback below would silently dispatch the default
    // profile's model instead of the one the operator named.
    if let Some(req) = profile_override {
        if let Some(msg) = loaded.registry.quarantine_error_for(req) {
            bail!(msg);
        }
    }

    // (#1054) Resolve the active profile: the CLI `--profile` override is
    // tried first, then the registry's `default_profile`. A `--profile` that
    // names a profile NOT defined on this machine falls back to
    // `default_profile` (the machine-agnostic-caller contract — a workflow
    // names the profile it wants; each machine maps it to a lab-validated
    // model or degrades to its default). When nothing resolves, probe.
    let (active_name, profile) = match loaded.registry.resolve_active(profile_override) {
        Some(pair) => {
            // Surface the fallback so the operator isn't surprised which model
            // ran: an explicit `--profile X` that resolved to a different name
            // means X wasn't defined here.
            if let Some(req) = profile_override {
                if req != pair.0 {
                    eprintln!(
                        "darkmux dispatch: requested profile `{req}` is not defined \
                         on this machine; using default_profile `{}` instead. Define \
                         `{req}` in ~/.darkmux/profiles.json to select a profile-specific \
                         model. (#1054)",
                        pair.0
                    );
                }
            }
            pair
        }
        None => {
            // (#1282) A quarantined `default_profile` is a hard stop with
            // the entry's own parse error — falling through to
            // `probe_loaded_model()` would dispatch against whatever
            // LMStudio happens to have loaded, exactly the contamination
            // the #1269 registry-load hard stop above exists to prevent.
            if let Some(default_name) = loaded.registry.default_profile.as_deref() {
                if let Some(msg) = loaded.registry.quarantine_error_for(default_name) {
                    bail!(msg);
                }
            }
            eprintln!(
                "darkmux dispatch: no usable profile (no --profile match and no \
                 default_profile set/defined); falling back to probe_loaded_model() — \
                 deprecated, set default_profile in ~/.darkmux/profiles.json. (#450 refactor 1b)"
            );
            return probe_loaded_model();
        }
    };

    // (#590 phase 2) Build a skill lookup so select_model can compose the
    // role's requested capability vector (role → skills → CapabilityProfile).
    // Skills unavailable ⇒ empty lookup ⇒ select_model takes its
    // default-model fallback (safe + behavior-preserving; #601).
    let skill_index: std::collections::HashMap<String, crate::types::Skill> =
        crate::loader::load_skills()
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect();
    match select_model(role, profile, |id| skill_index.get(id)) {
        Ok(id) => {
            // (#1135) Load the selected model at the profile's DECLARED n_ctx
            // before dispatch — and before the #408 cross-check below, which
            // then finds it resident. Pre-#1135 the dispatch only resolved the
            // model *id* and let LMStudio JIT-load it at the MODEL default
            // (e.g. 4096 on devstral), silently truncating large inputs (a
            // pr-review diff overflows 4096 → garbage review, no error). The
            // profile *declares* the context; honor it.
            if !skip_lmstudio_residency {
                if let Some(pm) = profile.models.iter().find(|m| m.id == id) {
                    ensure_model_loaded_at_ctx(pm)?;
                }
            }
            // (#450 review note / #408) Cross-check against actual
            // LMStudio loaded models. Residents loaded for one profile (a
            // prior dispatch, or a hand `lms load`) don't update
            // `default_profile` in the registry — so this path could select
            // `balanced`'s default model while LMStudio holds `fast`'s
            // models. The dispatch would then fail at the LMStudio call
            // (or worse, silently route to a different model if the id
            // collides). Surfacing the mismatch here makes the
            // misconfiguration operator-visible at dispatch time, not at
            // LMStudio's cryptic "model not loaded" error.
            if !skip_lmstudio_residency {
                if let Ok(loaded_ids) = probe_loaded_model_list() {
                if !loaded_ids.is_empty() && !loaded_ids.iter().any(|m| m == &id) {
                    let loaded = loaded_ids.join(", ");
                    if strict_selection_enabled() {
                        // (#408) Strict mode: a selected-vs-loaded
                        // mismatch is the dispatch-contamination case from
                        // the `feedback_model_unload_load_authority` memory
                        // note — proceeding risks measuring or attributing
                        // the wrong model, inheriting class-wide errors
                        // into every downstream claim. In a methodology /
                        // CI run the operator opts into hard-fail rather
                        // than a silent route to whatever LMStudio has
                        // loaded.
                        bail!(
                            "darkmux dispatch: profile `{active_name}` selects \
                             `{id}`, but LMStudio has loaded [{loaded}] and \
                             DARKMUX_STRICT_SELECTION is set — refusing to dispatch \
                             against an unselected model. Fix: `lms load {id}` to load \
                             the selected model, update `default_profile` to match \
                             what's loaded, or unset DARKMUX_STRICT_SELECTION to \
                             proceed anyway. (#408)"
                        );
                    }
                    eprintln!(
                        "darkmux dispatch: WARNING — profile `{active_name}` \
                         selects `{id}`, but LMStudio has loaded [{loaded}]. \
                         Residents loaded for another profile don't update \
                         `default_profile` in the registry; your loaded model \
                         won't match the selection. To fix: either `lms load {id}` \
                         to align LMStudio with the registry's default, or update \
                         `default_profile` to match what's loaded. Set \
                         DARKMUX_STRICT_SELECTION=1 to make this mismatch fatal \
                         instead of a warning. (#450 review note, #408)"
                    );
                }
                }
            }
            eprintln!(
                "darkmux dispatch: selected model `{id}` via profile `{active_name}`"
            );
            Ok(id)
        }
        Err(e) => {
            eprintln!(
                "darkmux dispatch: select_model error ({e}); falling back \
                 to probe_loaded_model() — deprecated. Add a default \
                 model to profile `{active_name}` to migrate. (#450 refactor 1b)"
            );
            // TODO(#450 phase-1c): the selected-vs-loaded MISMATCH case
            // now honors `DARKMUX_STRICT_SELECTION` (see the Ok branch
            // above). This Err branch — no model configured at all —
            // still warn-and-probes for back-compat with pre-refactor-1b
            // configs. When phase-1c lands the two-instances-per-purpose
            // policy, fold this fallback under strict mode too.
            probe_loaded_model()
        }
    }
}

/// (#590) Best-effort: the machine's registered utility model
/// (`internal.utility`), for overlaying onto the compactor. `None` if the
/// registry isn't loadable or no utility model is registered — the runtime
/// then keeps its built-in default compactor. Mirrors the loud-but-soft
/// posture of `resolve_dispatch_model_internal`: a missing binding is not an
/// error, just an absent overlay.
fn resolve_utility_model_internal(config_path: Option<&str>) -> Option<String> {
    darkmux_profiles::profiles::load_registry(config_path)
        .ok()
        .and_then(|l| l.registry.utility_model_id().map(str::to_string))
}

/// (#632) Resolve the context window the runtime needs to compute its
/// compaction threshold, from the active profile's default model.
/// Mirrors the profile resolution in `resolve_dispatch_model_internal` (CLI
/// `--profile` override > registry `default_profile`) and delegates the
/// derivation to `CompactionDispatchArgs::from_profile` so the
/// default-model → `n_ctx` rule has a single source of truth. Returns
/// `Ok(None)` when the registry/profile can't be resolved — the same edge
/// cases that send model selection to `probe_loaded_model()` — and `Err`
/// when the requested (or default) profile is QUARANTINED (#1282): the
/// window must never silently come from a DIFFERENT profile than the one
/// the dispatch names.
// `pub` so the mission-run brief path can size its proportional injected-context
// budget (#1011) from the SAME profile resolver the runtime uses for its
// compaction window. They share the resolver; a profile that declares no
// `context_window` falls back independently on each side (the budget to its own
// default), so they agree whenever the profile actually declares a window.
pub fn resolve_context_window_internal(
    profile_override: Option<&str>,
    config_path: Option<&str>,
) -> Result<Option<u32>> {
    let Ok(loaded) = darkmux_profiles::profiles::load_registry(config_path) else {
        return Ok(None);
    };
    // (#1282) Quarantine-aware, mirroring `resolve_dispatch_model_internal`:
    // a quarantined requested profile hard-fails instead of taking the
    // #1054 default fallback below.
    if let Some(req) = profile_override {
        if let Some(msg) = loaded.registry.quarantine_error_for(req) {
            bail!(msg);
        }
    }
    // (#1054) Same graceful resolution as model selection — a requested profile
    // undefined here falls back to default_profile, so the context window comes
    // from the SAME profile the model does.
    let Some((_active_name, profile)) = loaded.registry.resolve_active(profile_override) else {
        // (#1282) A quarantined `default_profile` hard-fails rather than
        // reporting "no window" for a profile that IS in the file, broken.
        if let Some(default_name) = loaded.registry.default_profile.as_deref() {
            if let Some(msg) = loaded.registry.quarantine_error_for(default_name) {
                bail!(msg);
            }
        }
        return Ok(None);
    };
    Ok(crate::dispatch::CompactionDispatchArgs::from_profile(profile).context_window)
}

/// (#632) Guard that the runtime always receives a context window. Some
/// dispatch paths (bare `dispatch`, the lab `prompt` provider) build a
/// `default()` `CompactionDispatchArgs` with `context_window: None`, but the
/// runtime has no built-in default and hard-errors without one. Fill it from
/// the profile-derived `fallback`; a path that already set a window (e.g. the
/// coding-task provider via `from_profile`) is left untouched. Pure so the
/// guard rule is unit-testable without resolving a registry or spawning a
/// container.
fn ensure_context_window(
    compaction: &mut crate::dispatch::CompactionDispatchArgs,
    fallback: Option<u32>,
) {
    if compaction.context_window.is_none() {
        compaction.context_window = fallback;
    }
}

// (#1280) `utility_preflight_warning` (the #590 warn-only preflight) was
// retired: the utility/compactor model is now ENSURED resident at its declared
// context (namespaced) via `ensure_model_loaded_at_ctx` at dispatch start, the
// same guard the dispatch model gets — not merely warned about.

/// (#1280) Build the utility/compactor model's residency spec (the compaction
/// CONTEXT WINDOW is its required `n_ctx` — a compaction payload is sized to
/// that window, so the model must be loaded at least that large). Pure; the
/// wiring's unit-test seam.
fn utility_residency_pm(util_id: &str, context_window: u32) -> darkmux_types::ProfileModel {
    darkmux_types::ProfileModel {
        id: util_id.to_string(),
        n_ctx: Some(context_window),
        ..Default::default()
    }
}

/// (#1280) Ensure the utility/compactor model is resident at the compaction
/// context window via `load` (production: `ensure_model_loaded_at_ctx`, which
/// loads under the `darkmux:` namespace with the bounded #1139 machinery).
/// Returns `Some(warning)` on a load failure — WARN, never abort: a
/// compaction-less short dispatch still runs; the warning names the risk
/// (JIT-load at the model default, truncated summaries). `None` when there is
/// no compactor, no window, or the load succeeded. Pure over the injected
/// `load` so the warn-not-abort contract is unit-testable without LMStudio.
fn ensure_utility_resident(
    compactor_model: Option<&str>,
    context_window: Option<u32>,
    load: impl Fn(&darkmux_types::ProfileModel) -> Result<()>,
) -> Option<String> {
    let util_id = compactor_model?;
    let window = context_window?;
    let util_pm = utility_residency_pm(util_id, window);
    match load(&util_pm) {
        Ok(()) => None,
        Err(e) => Some(format!(
            "darkmux dispatch: WARNING — utility/compactor model `{util_id}` could not be \
             ensured resident at n_ctx={window}: {e:#}. Compaction may JIT-load it at the \
             model default and truncate its summaries. (#1280)"
        )),
    }
}

/// (#408) Whether a selected-vs-loaded model mismatch should be fatal.
///
/// Opt-in via `DARKMUX_STRICT_SELECTION` (truthy: `1` / `true` / `yes` /
/// `on`, case-insensitive). Default off keeps back-compat: a mismatch
/// warns but proceeds, since LMStudio may JIT-load the selected model and
/// the operator owns loaded state (operator-sovereignty). Strict mode is
/// the methodology-grade setting — in lab / CI runs where dispatching
/// against the wrong model contaminates downstream measurement claims
/// (#408), the mismatch should hard-fail rather than silently route.
fn strict_selection_enabled() -> bool {
    // env(DARKMUX_STRICT_SELECTION) truthy > config.runtime.strict_selection >
    // false (#661 Slice 4).
    darkmux_types::config_access::strict_selection()
}

/// (#450 review note) Return the list of currently-LOADED LMStudio
/// model identifiers — both `modelKey` (bare, what profiles reference)
/// and `identifier` (namespaced, what darkmux-loaded models surface as).
///
/// Uses `lms ps --json` because LMStudio's `/v1/models` endpoint
/// returns the full CATALOG (including unloaded models), not the
/// loaded set — so `/v1/models` would silently match any catalogued
/// model and defeat the cross-check.
///
/// Returns `Err` on probe failure (caller treats as "can't validate"
/// and proceeds without the warning). The function intentionally
/// returns BOTH modelKey and identifier so a profile that names
/// either form (`qwen3.6-35b-a3b-turboquant-mlx` OR
/// `darkmux:qwen3.6-35b-a3b-turboquant-mlx`) can match correctly.
/// (#1135) Ensure the dispatch's selected profile model is resident in
/// LMStudio AT the profile's declared `n_ctx` before the dispatch runs.
///
/// Pre-#1135 the dispatch sent chat-completions to a model id and let LMStudio
/// JIT-load it at the MODEL default context (e.g. 4096 on devstral) — silently
/// truncating large inputs (a pr-review diff overflows 4096 → garbage review,
/// with no error; a tiny smoke message fits 4096 so the defect hides). Here
/// darkmux loads it at the declared context:
///   - already resident at `>= n_ctx` → reuse (no churn, even if user-loaded);
///   - resident at a SMALLER context → unload + reload at `n_ctx` (standing
///     unload/load authority for dispatch correctness, #408 / the
///     model-unload-load-authority operator note);
///   - not resident → load at `n_ctx` under the `darkmux:<id>` namespace.
///
/// A load failure (insufficient RAM / LMStudio load timeout) returns a clear,
/// operator-actionable error rather than degrading silently to the default —
/// the early-detection half of #1139 (the fallback/eviction half is #1139/#1140).
fn ensure_model_loaded_at_ctx(pm: &darkmux_types::ProfileModel) -> Result<()> {
    use darkmux_profiles::{lms, swap};
    // (#1282) This is a LOCAL load path (remote-brained dispatches skip it) —
    // a model without a declared `n_ctx` is a resolution error here, with the
    // uniform require_n_ctx message, never a parse-stage failure.
    let n_ctx = pm.require_n_ctx()?;
    let loaded = lms::list_loaded().unwrap_or_default();
    match loaded.iter().find(|m| m.model == pm.id) {
        Some(m) if m.context >= u64::from(n_ctx) => return Ok(()),
        Some(m) => {
            eprintln!(
                "darkmux dispatch: `{}` is resident at context {} but the profile \
                 declares n_ctx={}; reloading at {} so the dispatch gets the declared \
                 context. (#1135)",
                pm.id, m.context, n_ctx, n_ctx
            );
            lms::unload(&m.identifier).with_context(|| {
                format!("unloading `{}` to reload at n_ctx={}", m.identifier, n_ctx)
            })?;
        }
        None => {
            eprintln!(
                "darkmux dispatch: loading `{}` at n_ctx={} (the profile's declared \
                 context) before dispatch. (#1135)",
                pm.id, n_ctx
            );
        }
    }
    let identifier = swap::namespaced_identifier(pm);
    load_at_ctx_bounded(&pm.id, &identifier, n_ctx)
}

/// (#1139) Load `model_key` under `identifier` at `n_ctx` through the bounded
/// [`LmsHost`] `ModelHost` port (#1276) instead of the raw, uncapped
/// `lms::load_with_identifier` (`Command::status`, which blocks indefinitely on
/// a RAM-starved / stuck load until the workflow's outer kill). The port
/// enforces the resolved model-load deadline and classifies the failure into a
/// TYPED error, so a RAM-exhausted load becomes a clear, operator-actionable
/// message — never a hang or a silent degrade.
fn load_at_ctx_bounded(model_key: &str, identifier: &str, n_ctx: u32) -> Result<()> {
    use darkmux_gestalt::{Deadline, ModelHost};
    let mut host = darkmux_profiles::gestalt_host::LmsHost::new();
    let deadline = Deadline::from_secs(darkmux_types::config_access::model_load_timeout_seconds());
    map_load_result(host.load(model_key, identifier, n_ctx, deadline), model_key, n_ctx)
}

/// (#1139) Map the bounded [`ModelHost::load`] outcome to an operator-actionable
/// `anyhow` result — the RAM-exhaustion case becomes a clear, named error, not a
/// hang or silent degrade. Pure so the message mapping is unit-testable without
/// a live host.
fn map_load_result(
    result: Result<darkmux_gestalt::LoadReport, darkmux_gestalt::HostError>,
    model_key: &str,
    n_ctx: u32,
) -> Result<()> {
    use darkmux_gestalt::HostError;
    match result {
        Ok(_report) => Ok(()),
        Err(HostError::InsufficientResources { detail }) => bail!(
            "darkmux: model `{model_key}` could not load at n_ctx={n_ctx} — insufficient RAM \
             ({detail}). Free resources, lower the profile's n_ctx, or evict a resident \
             darkmux model (`darkmux machine eject`), then retry. (#1139)"
        ),
        Err(HostError::Timeout { phase, waited }) => bail!(
            "darkmux: model `{model_key}` load did not finish within the bounded {phase} \
             deadline ({waited:?}) — the load is likely stuck (wrong id, waiting on a \
             download, or a RAM-starved host). Raise DARKMUX_MODEL_LOAD_TIMEOUT_SECONDS if \
             the machine is just slow. (#1139/#1276)"
        ),
        Err(HostError::UnknownModel { model_key }) => bail!(
            "darkmux: model `{model_key}` was not found by LMStudio at load time — check the \
             id against `lms ls`. (#1139)"
        ),
        Err(e) => bail!(
            "darkmux: loading `{model_key}` at n_ctx={n_ctx} failed: {e}. Likely insufficient \
             RAM or an LMStudio load error. (#1139)"
        ),
    }
}

fn probe_loaded_model_list() -> Result<Vec<String>> {
    let output = Command::new("lms")
        .args(["ps", "--json"])
        .output()
        .context("running `lms ps --json` to enumerate loaded models")?;
    if !output.status.success() {
        bail!(
            "`lms ps --json` failed (exit {})",
            output.status.code().unwrap_or(-1)
        );
    }
    let body: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("parsing `lms ps --json` output")?;
    let mut ids = Vec::new();
    if let Some(arr) = body.as_array() {
        for entry in arr {
            // Both forms a profile id could match against.
            if let Some(key) = entry["modelKey"].as_str() {
                ids.push(key.to_string());
            }
            if let Some(ident) = entry["identifier"].as_str() {
                ids.push(ident.to_string());
            }
        }
    }
    Ok(ids)
}

/// return the first model id. Uses curl so we don't drag a Rust HTTP
/// client dep into darkmux's main crate for one probe call.
fn probe_loaded_model() -> Result<String> {
    // env(DARKMUX_LMSTUDIO_URL) > config.lmstudio_url > http://localhost:1234,
    // + the /v1/models path (#661 Slice 4 — the probe is now config-aware and
    // shares the base URL with the phase chat narrator).
    let url = format!("{}/v1/models", darkmux_types::config_access::lmstudio_url());
    let output = Command::new("curl")
        .args(["-sf", "-m", "5", &url])
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

// `first_user_symlink_in` and `is_macos_firmlink` moved to
// `darkmux_types::workdir` as part of Wave-E.2 (#255). Runners + both runtime
// paths now share one implementation via `workdir::validate_workdir`.

/// Map a role's tool_palette (allow/deny in role-vocab) to the list of
/// runtime-vocab tool names that should be exposed to the model via
/// `--allowed-tools`.
///
/// Role-vocab and runtime-vocab don't align 1-to-1:
///   - role "read" → runtime ["read", "search"] (search is a specialized
///     read; "you may read files" implies it)
///   - role "edit" → runtime ["edit"]
///   - role "write" → runtime ["write"]
///   - role "exec" → runtime ["bash"]
///   - role "process", "update_plan" → no runtime equivalent (silently
///     dropped; no runtime tool implements these concepts today)
///
/// Allow first, then deny removes. Deny wins on conflict. Unknown
/// role-vocab tokens are silently dropped — keeps forward-compatibility
/// for roles that name tools the runtime doesn't yet implement.
///
/// Returns `None` when the palette is empty (no allow, no deny) so the
/// caller can decide between "fail loud (no tools)" and "back-compat
/// default (full catalog)." Today the caller passes `None` →
/// runtime's `--allowed-tools` flag is omitted → runtime exposes full
/// catalog. The empty-palette case usually means "role definition is
/// incomplete," not "no tools allowed."
/// All role-vocab tokens darkmux currently knows how to map to
/// runtime tools. Source of truth for `role_to_runtime` (mapping)
/// AND `unknown_role_vocab_tokens` (typo detection). When the
/// runtime gains a new tool or roles gain a new capability token,
/// add it here AND to the match in `role_to_runtime`.
const KNOWN_ROLE_VOCAB: &[&str] = &["read", "edit", "write", "exec", "process", "update_plan"];

/// Single source of truth for role-vocab → runtime-vocab. Add new
/// mappings here when the runtime gains a new tool or roles gain
/// new capability tokens. Unknown tokens return an empty slice;
/// detection of unknowns lives in `unknown_role_vocab_tokens` so
/// the caller can warn loudly (#340).
fn role_to_runtime(role_name: &str) -> &'static [&'static str] {
    match role_name {
        "read" => &["read", "search"],
        "edit" => &["edit"],
        "write" => &["write"],
        "exec" => &["bash"],
        // Known role-vocab tokens with no runtime equivalent today.
        // NOT typos — silently dropped is correct behavior.
        "process" | "update_plan" => &[],
        // Truly unknown. Empty result here; the caller should run
        // `unknown_role_vocab_tokens` separately to surface this as
        // an operator-visible warning (#340 — silent drops are
        // typo-prone). Don't bail here; doing so would break valid
        // dispatches when a future role manifest references a token
        // we haven't wired yet.
        _ => &[],
    }
}

/// Tokens in a role's tool_palette that aren't in [`KNOWN_ROLE_VOCAB`]
/// — typos, future tokens, vendor-specific names. Returned sorted +
/// deduplicated. Empty when the palette only contains known tokens.
///
/// Caller pattern: call before dispatch; if non-empty, warn the
/// operator with the unknown tokens + the list of known ones so
/// they can correct the manifest. Doctor uses the same helper to
/// surface unknowns across all role manifests proactively (#340).
pub fn unknown_role_vocab_tokens(palette: &crate::types::ToolPalette) -> Vec<String> {
    let mut unknowns: Vec<String> = palette
        .allow
        .iter()
        .chain(palette.deny.iter())
        .filter(|name| !KNOWN_ROLE_VOCAB.contains(&name.as_str()))
        .cloned()
        .collect();
    unknowns.sort();
    unknowns.dedup();
    unknowns
}

fn compute_runtime_allowed_tools(palette: &crate::types::ToolPalette) -> Option<Vec<String>> {
    // Empty palette: caller decides; today we return None so the
    // runtime exposes its full catalog (back-compat).
    if palette.allow.is_empty() && palette.deny.is_empty() {
        return None;
    }

    // Allow first.
    let mut allowed: Vec<String> = palette
        .allow
        .iter()
        .flat_map(|name| role_to_runtime(name).iter().map(|s| s.to_string()))
        .collect();

    // Deny removes. Deny wins on conflict.
    let denied: Vec<String> = palette
        .deny
        .iter()
        .flat_map(|name| role_to_runtime(name).iter().map(|s| s.to_string()))
        .collect();
    allowed.retain(|t| !denied.contains(t));

    // Dedupe while preserving order (a role manifest could allow both
    // "read" and "edit" — `read` brings ["read","search"] and we don't
    // want duplicates of either if some future mapping overlaps).
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    allowed.retain(|t| seen.insert(t.clone()));

    Some(allowed)
}

/// Comma-separated list of all known role-vocab tokens, for use in
/// operator-facing warning messages (#340). Wrapped in a helper so
/// the formatting stays consistent across dispatch + doctor surfaces.
pub fn known_role_vocab_csv() -> String {
    KNOWN_ROLE_VOCAB.join(", ")
}

#[cfg(test)]
#[path = "dispatch_internal_tests.rs"]
mod tests;
