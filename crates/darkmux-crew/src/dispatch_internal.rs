//! Internal runtime dispatch path.
//!
//! Routes a `darkmux crew dispatch <role>` invocation to the
//! `darkmux-runtime` docker container. Per-dispatch container, mounted
//! workspace, structured output collected from stdout.
//!
//! Default runtime as of the runtime-default flip. Openclaw remains
//! available via the explicit `--runtime openclaw` flag for operators
//! who already have it installed and configured.
//!
//! Deliberately simpler than the openclaw path:
//!
//! - No openclaw pre-flight (it's not involved)
//! - No `--workdir` symlink injection (workspace is a fresh tempdir
//!   per dispatch; the gallery-incident class of bug is structurally
//!   impossible because there's nowhere persistent to leak into)
//! - No sprint-output persistence (later iteration)
//! - No watched-path post-dispatch echo (same)
//! - No model pin enforcement (probes whatever LMStudio currently has loaded)
//!
//! See `runtime/` for the container image this dispatches to.

use crate::dispatch::DispatchResult;
use crate::dispatch::DispatchOpts;
use crate::loader::{load_autonomous_dispatch_preamble, load_role_prompt, load_roles};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
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
    eprintln!("darkmux crew dispatch: no local runtime image — pulling `{image}` from GHCR (one-time, #759)…");
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
             docker build -t {RUNTIME_IMAGE} runtime/\n  \
             - Or use `--runtime openclaw` if you have openclaw installed."
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
        "darkmux crew dispatch: cached runtime binary → {} (from {source_image}, for --image injection)",
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
    if let Some(n) = darkmux_types::config_access::max_turns() {
        cmd.arg("--max-turns").arg(n.to_string());
    }
    if let Some(n) = darkmux_types::config_access::max_tokens() {
        cmd.arg("--max-tokens").arg(n.to_string());
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

    if config.json {
        args.push("--json".to_string());
    }

    if let Some(allowed) = config.allowed_tools.as_ref() {
        args.push("--allowed-tools".to_string());
        args.push(allowed.join(","));
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
                "darkmux crew dispatch: {var}=`{raw}` is not a positive integer; \
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

/// (#717) Bookend guard for the internal dispatch lifecycle. Once
/// `dispatch.start` is emitted, the dispatch can still `?`-return before the
/// clean `dispatch.complete` (runtime-binary extraction, container spawn, or
/// `wait_with_output` failing) — or panic. Without a terminal record that
/// leaves an orphaned start; since #714 stamped `mission_id` on it, the
/// orphan now groups under its mission and would render as perpetually
/// in-flight. This guard fires a `dispatch.error` terminal record on `Drop`
/// unless `disarm`ed, so every start has a matching terminal event. The clean
/// path (and the container-ran-but-failed path, which already emits its own
/// `dispatch.error`) calls `disarm()` after that emit, so the guard never
/// double-counts a dispatch that reached its own terminal record.
struct DispatchBookendGuard {
    armed: bool,
    role_id: String,
    session_id: String,
    model: String,
    mission_id: Option<String>,
    sprint_id: Option<String>,
}

impl DispatchBookendGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DispatchBookendGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Best-effort, same as every other emit on this path: a flow-sink
        // write problem must not mask the original error propagating out.
        let _ = darkmux_flow::record(crate::dispatch::build_dispatch_record_with_payload(
            darkmux_flow::Level::Error,
            "dispatch error",
            &self.role_id,
            &self.session_id,
            Some(&self.model),
            self.mission_id.as_deref(),
            self.sprint_id.as_deref(),
            Some(serde_json::json!({
                "runtime": "internal",
                "result_class": "error",
                "error": "dispatch terminated before completion (early return or panic)",
            })),
        ));
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

pub fn dispatch(opts: DispatchOpts) -> Result<DispatchResult> {
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
        "darkmux crew dispatch: runtime=internal — image: {image}{}",
        if inject {
            " (darkmux-runtime binary injected)"
        } else {
            ""
        }
    );

    // 1. Load the role manifest + .md prompt. The internal runtime uses
    //    the SAME on-disk role definition as the openclaw path so the
    //    prompts stay identical across runtimes — load-bearing for the
    //    runtime-vs-openclaw comparison.
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
    // #340 — surface unknown role-vocab tokens loudly. Unknown tokens
    // (typos like "exce" for "exec", future tokens not yet wired)
    // get silently dropped by `role_to_runtime`; without this warning
    // the operator sees `tool_palette filtered to []` and has no
    // signal about WHY their role got zero tools.
    let unknown_tokens = unknown_role_vocab_tokens(&role.tool_palette);
    if !unknown_tokens.is_empty() {
        eprintln!(
            "darkmux crew dispatch: role `{}` declares unknown tool-vocab tokens: [{}] \
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
    let model = resolve_dispatch_model_internal(
        role,
        opts.profile_name.as_deref(),
        opts.config_path.as_deref(),
    )
    .context(
        "model selection failed. Ensure `~/.darkmux/profiles.json` has \
         a profile with at least one model (the default model is \
         `default_model` or the first model in `models`), or load a model in \
         LMStudio (darkmux swap <profile>) as the deprecated fallback."
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

    // (#714) Resolve the sprint → mission once so every flow record this
    // dispatch emits (start / turn / tool / compaction / complete / telemetry)
    // carries `mission_id`/`sprint_id` and groups under its mission in the
    // observability view. Best-effort: None when this isn't a sprint-bound
    // dispatch. The router (dispatch.rs) returns here before its own sprint
    // wiring, so the internal path resolves it directly from `opts`.
    let mission_id = crate::dispatch::resolve_mission_for_sprint(opts.sprint_id.as_deref());
    let sprint_id = opts.sprint_id.clone();

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
    //    after the container exits. That's half the point of replacing
    //    the openclaw workspace model (operator visibility into what
    //    the dispatch did).
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
        "darkmux crew dispatch: workspace={} ({})",
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
        "darkmux crew dispatch: out-dir={} (runtime bookkeeping → /darkmux-out)",
        host_out.display()
    );

    // (#386) Write the user message to a file in the (already-mounted) out-dir
    // and hand the runtime `--prompt-file` instead of `--prompt <text>`, so a
    // substantial brief never lands on the `docker run` command line — where it
    // would both hit ARG_MAX (the very case `--message-from-file` exists for)
    // and be visible in `ps`. The container reads it back from the bind mount.
    fs::write(host_out.join(PROMPT_FILE_NAME), &opts.message)
        .with_context(|| format!("writing dispatch prompt file under {}", host_out.display()))?;

    // 5. Emit dispatch.start flow record with runtime metadata in payload
    //    (#204). Pairs with dispatch.complete below via session_id, same
    //    as the openclaw path does.
    let dispatch_start_payload = serde_json::json!({
        "runtime": "internal",
        // (#1126) The resolved runtime image (operator `--image` or the default
        // darkmux image, line ~711) — the environment the coder ran in. The
        // viewer's run brief + recent-runs rail read `payload.image`; it was a
        // dead reference until now (no path emitted it). openclaw dispatches
        // have no container image, so that path omits the field honestly.
        "image": image.clone(),
        "prompt_chars": opts.message.chars().count(),
        "system_chars": system_prompt.chars().count(),
        "workspace": workspace.display().to_string(),
    });
    let _ = darkmux_flow::record(crate::dispatch::build_dispatch_record_with_payload(
        darkmux_flow::Level::Info,
        "dispatch start",
        &opts.role_id,
        &session_id,
        Some(&model),
        mission_id.as_deref(),
        sprint_id.as_deref(),
        Some(dispatch_start_payload),
    ));
    // (#717) Arm the bookend guard: from here, any `?`-return or panic before
    // the clean `dispatch.complete` below emits a `dispatch.error` terminal so
    // the start is never left orphaned (and mission-grouped-but-in-flight).
    let mut bookend = DispatchBookendGuard {
        armed: true,
        role_id: opts.role_id.clone(),
        session_id: session_id.clone(),
        model: model.clone(),
        mission_id: mission_id.clone(),
        sprint_id: sprint_id.clone(),
    };
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
        resolve_context_window_internal(opts.profile_name.as_deref(), opts.config_path.as_deref()),
    );

    // (#590) Pre-compaction loaded-check: warn (don't abort) if the resolved
    // compactor model is registered but not resident in LMStudio, BEFORE a
    // mid-dispatch compaction call fails. Best-effort — skipped when lms is
    // unreachable. Host-side I/O, kept out of the pure argv builder.
    if let Some(util_id) = compaction.compactor_model.as_deref() {
        if let Ok(loaded) = darkmux_profiles::lms::list_loaded() {
            if let Some(warning) = utility_preflight_warning(util_id, &loaded) {
                eprintln!("{warning}");
            }
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
    // change. This sprint scopes to caps/limits/user only.

    // (#457 Changes 2+3) Operator-opt-in per-dispatch caps on turn
    // count + cumulative completion tokens. Read from env vars on
    // host side; pass via --max-turns / --max-tokens CLI flags to
    // the runtime container. Both default unlimited (omitted flag
    // → runtime's `Option<u32>` stays None → no cap applied).
    apply_runtime_limit_flags(&mut cmd);

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let child = cmd.spawn().context("spawning darkmux-runtime container")?;

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
    let tailer_handle = {
        let stop = Arc::clone(&stop_flag);
        // (#out-of-band) The trajectory now lands in the out-dir, not the
        // workspace. The tailer reads from there.
        let out_dir = host_out.clone();
        let session_id = session_id.clone();
        let role_id = opts.role_id.clone();
        let model = model.clone();
        let mission_id = mission_id.clone();
        let sprint_id = sprint_id.clone();
        let inactivity_deadline = Arc::clone(&inactivity_deadline);
        thread::spawn(move || {
            run_tailer(
                out_dir,
                session_id,
                role_id,
                model,
                mission_id,
                sprint_id,
                stop,
                inactivity_deadline,
                inactivity_secs,
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

    // (#557 slice 4) Always-on lms + container-CPU telemetry sampler.
    // Mirrors the tailer/watchdog lifecycle: a background thread that
    // loops until `sampler_stop` is set (after `wait_with_output()`
    // returns) + a best-effort `.join()`. Per `TELEMETRY_SAMPLE_INTERVAL`
    // it samples two host-side surfaces and forwards each into the one
    // flow stream as a `category=telemetry` record:
    //   - `source=lms`     → load/unload deltas of the LMStudio loaded set
    //   - `source=process` → the per-dispatch container's CPU%
    // ALWAYS-ON (not `--instrument`-gated). Replaces the OC-gateway CPU
    // sampler vestige for the crew path — CPU is sourced from `docker
    // stats <container>`, never from a gateway process / OPENCLAW_GATEWAY_PORT.
    // Best-effort throughout: a failed `list_loaded` / `docker stats` /
    // record never panics or aborts the dispatch — it's additive
    // observability, so starting/stopping it is non-load-bearing.
    let sampler_stop = Arc::new(AtomicBool::new(false));
    let sampler_handle = {
        let sampler_stop = Arc::clone(&sampler_stop);
        let role_id = opts.role_id.clone();
        let session_id = session_id.clone();
        let model = model.clone();
        let mission_id = mission_id.clone();
        let sprint_id = sprint_id.clone();
        let container_name = container_name.clone();
        thread::spawn(move || {
            run_telemetry_sampler(
                sampler_stop,
                role_id,
                session_id,
                model,
                mission_id,
                sprint_id,
                container_name,
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
    let dispatch_complete_payload = serde_json::json!({
        "runtime": "internal",
        "wall_ms": wall_ms,
        "stdout_chars": stdout.chars().count(),
        "stderr_chars": stderr.chars().count(),
        "exit_code": exit_code,
        "result_class": if exit_code == 0 { "ok" } else { "error" },
        "total_turns": trajectory_summary.turns,
        "total_tools": trajectory_summary.tool_calls,
        "total_compactions": trajectory_summary.compactions,
        "prompt_tokens": tokens.prompt,
        "completion_tokens": tokens.completion,
        "total_tokens": tokens.total(),
    });
    let (action, level) = if exit_code == 0 {
        ("dispatch complete", darkmux_flow::Level::Info)
    } else {
        ("dispatch error", darkmux_flow::Level::Error)
    };
    let _ = darkmux_flow::record(crate::dispatch::build_dispatch_record_with_payload(
        level,
        action,
        &opts.role_id,
        &session_id,
        Some(&model),
        mission_id.as_deref(),
        sprint_id.as_deref(),
        Some(dispatch_complete_payload),
    ));
    // (#717) Terminal record emitted (clean complete, or the container-ran-
    // but-failed `dispatch.error` above) — disarm so the guard doesn't emit a
    // second terminal on the function's normal return.
    bookend.disarm();
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
        sprint_id.as_deref(),
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
        watched_state: Vec::new(),
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
    sprint_id: Option<String>,
    stop_flag: Arc<AtomicBool>,
    inactivity_deadline: Arc<Mutex<Instant>>,
    inactivity_secs: u64,
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
    .with_mission(mission_id, sprint_id);

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

/// (#557 slice 4) Run the always-on lms + container-CPU telemetry
/// sampler to completion. Loops until `stop_flag` is set (the main thread
/// sets it after `wait_with_output()` returns), sampling two host-side
/// surfaces each `TELEMETRY_SAMPLE_INTERVAL` and forwarding each
/// observation into the one flow stream as a `category=telemetry` record.
///
/// **lms (`source=lms`)**: `darkmux_profiles::lms::list_loaded()` is the
/// lms-ps source. Each tick diffs the current loaded set against the
/// previous tick's via `telemetry_sampler::lms_diff`; only CHANGES emit
/// (`{event:"load"|"unload", model, gb?}`).
///
/// **`prev` seeding choice**: `prev` is seeded with the FIRST sample
/// (`prev = Vec::new()` initially, but the first iteration computes a diff
/// against an empty `prev` ONLY when we want the initial set as loads).
/// We chose to **seed `prev` on the first iteration and emit nothing** —
/// the models already resident when the dispatch starts are pre-existing
/// state, not a load this dispatch caused, so emitting them as "load"
/// events would be spurious. Only loads/unloads that happen DURING the
/// dispatch surface. Implemented via the `seeded` flag: first tick sets
/// `prev = cur` and skips the diff; subsequent ticks diff normally.
///
/// **process (`source=process`)**: `docker stats <container> --no-stream
/// --format "{{.CPUPerc}}"` → `parse_cpu_percent` → `{cpu:<N>}`. This is
/// the OC-gateway-sampler replacement: CPU comes from the per-dispatch
/// container, never from a gateway process.
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
    sprint_id: Option<String>,
    container_name: String,
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
            sprint_id.as_deref(),
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
                // First successful sample: seed `prev`, emit nothing. The
                // already-resident models are pre-existing state, not a
                // load this dispatch caused.
                prev = cur;
                seeded = true;
            } else {
                for payload in crate::telemetry_sampler::lms_diff(&prev, &cur) {
                    emit("lms", "telemetry.lms", payload);
                }
                prev = cur;
            }
        }

        // Container CPU%. `docker stats --no-stream` is a single-shot
        // sample of THIS dispatch's container. Best-effort: any failure
        // (docker gone, container already exited, unparseable output) just
        // skips the CPU sample for this tick.
        if let Ok(out) = Command::new("docker")
            .args([
                "stats",
                &container_name,
                "--no-stream",
                "--format",
                "{{.CPUPerc}}",
            ])
            .output()
        {
            if out.status.success() {
                let raw = String::from_utf8_lossy(&out.stdout);
                if let Some(cpu) = crate::telemetry_sampler::parse_cpu_percent(&raw) {
                    emit(
                        "process",
                        "telemetry.process",
                        serde_json::json!({ "cpu": cpu }),
                    );
                }
            }
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
    /// (#714) Mission/sprint this dispatch belongs to (when sprint-bound),
    /// stamped onto every per-event flow record so they group under the
    /// mission in the observability view. `None` for a one-off dispatch.
    mission_id: Option<String>,
    sprint_id: Option<String>,
    last_heartbeat_at: Option<Instant>,
    summary: TrajectorySummary,
    /// (#457) Shared with the watchdog thread. Tailer writes a new
    /// deadline (`now + inactivity_secs`) when a `compaction` event
    /// fires; watchdog reads each tick to decide whether to kill the
    /// container. `None` in test fixtures that don't exercise the
    /// reset path.
    inactivity_deadline: Option<Arc<Mutex<Instant>>>,
    inactivity_secs: u64,
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
            sprint_id: None,
            last_heartbeat_at: None,
            summary: TrajectorySummary::default(),
            inactivity_deadline: Some(inactivity_deadline),
            inactivity_secs,
        }
    }

    /// (#714) Stamp the sprint → mission this dispatch belongs to so the
    /// per-event flow records group under the mission. Builder-style so the
    /// `new`/`new_for_test` call sites (incl. unit tests) stay unchanged;
    /// only the production `run_tailer` opts in.
    fn with_mission(mut self, mission_id: Option<String>, sprint_id: Option<String>) -> Self {
        self.mission_id = mission_id;
        self.sprint_id = sprint_id;
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
            sprint_id: None,
            last_heartbeat_at: None,
            summary: TrajectorySummary::default(),
            inactivity_deadline: None,
            inactivity_secs: 600,
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
            self.sprint_id.as_deref(),
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
            self.sprint_id.as_deref(),
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
            "darkmux's default runtime (`--runtime internal`) requires Docker, \
             but `docker version` failed:\n  {}\n\
             Options:\n  \
             - Start Docker Desktop, OR\n  \
             - Re-run with `--runtime openclaw` if you have openclaw installed",
            stderr
        ),
        DockerRuntimeStatus::BinaryMissing => bail!(
            "darkmux's default runtime (`--runtime internal`) requires Docker, \
             but the `docker` binary isn't on PATH.\n\
             Options:\n  \
             - Install Docker Desktop (https://www.docker.com/products/docker-desktop), OR\n  \
             - Re-run with `--runtime openclaw` if you have openclaw installed"
        ),
        DockerRuntimeStatus::ImageMissing => bail!(
            "no darkmux runtime image found locally. darkmux pulls the \
             version-pinned image `{}` from GHCR on demand; if that pull \
             can't run, build it once from a darkmux source checkout:\n  \
             docker build -t {RUNTIME_IMAGE} runtime/\n\
             (Or use `--runtime openclaw` if you have openclaw installed.)",
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
/// 3. On any failure (no registry, no default, no profile, no model),
///    log a deprecation warning + fall back to `probe_loaded_model()`.
///    Back-compat for pre-refactor-1b configurations; the warning
///    points the operator at the migration.
///
/// The fallback is intentional but loud. Per memory note
/// `feedback_model_unload_load_authority`, silent reliance on "whatever
/// LMStudio happens to have loaded" is the contaminating-dispatch
/// anti-pattern. The deprecation warning makes the misconfiguration
/// operator-visible while keeping pre-refactor-1b setups working.
fn resolve_dispatch_model_internal(
    role: &crate::types::Role,
    profile_override: Option<&str>,
    config_path: Option<&str>,
) -> Result<String> {
    use crate::select::select_model;
    use darkmux_profiles::profiles::load_registry;

    let loaded = match load_registry(config_path) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!(
                "darkmux crew dispatch: profile registry not loadable ({e}); \
                 falling back to probe_loaded_model() — deprecated, configure \
                 ~/.darkmux/profiles.json. (#450 refactor 1b)"
            );
            return probe_loaded_model();
        }
    };

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
                        "darkmux crew dispatch: requested profile `{req}` is not defined \
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
            eprintln!(
                "darkmux crew dispatch: no usable profile (no --profile match and no \
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
            // (#450 review note / #408) Cross-check against actual
            // LMStudio loaded models. `darkmux swap <name>` loads a
            // profile's models in LMStudio but does NOT update
            // `default_profile` in the registry — so after `swap fast`
            // with default `balanced`, this path would happily select
            // balanced's default model while LMStudio is loaded
            // with fast's models. The dispatch would then fail at the
            // LMStudio call
            // (or worse, silently route to a different model if the id
            // collides). Surfacing the mismatch here makes the
            // misconfiguration operator-visible at dispatch time, not at
            // LMStudio's cryptic "model not loaded" error.
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
                            "darkmux crew dispatch: profile `{active_name}` selects \
                             `{id}`, but LMStudio has loaded [{loaded}] and \
                             DARKMUX_STRICT_SELECTION is set — refusing to dispatch \
                             against an unselected model. Fix: `darkmux swap \
                             {active_name}` to load the selected model, update \
                             `default_profile` to match what's loaded, or unset \
                             DARKMUX_STRICT_SELECTION to proceed anyway. (#408)"
                        );
                    }
                    eprintln!(
                        "darkmux crew dispatch: WARNING — profile `{active_name}` \
                         selects `{id}`, but LMStudio has loaded [{loaded}]. \
                         `darkmux swap` does not update `default_profile` in the \
                         registry; if you swapped recently, your loaded model \
                         won't match the selection. To fix: either `darkmux swap \
                         {active_name}` to align LMStudio with the registry's \
                         default, or update `default_profile` to match what's \
                         loaded. Set DARKMUX_STRICT_SELECTION=1 to make this \
                         mismatch fatal instead of a warning. (#450 review note, #408)"
                    );
                }
            }
            eprintln!(
                "darkmux crew dispatch: selected model `{id}` via profile `{active_name}`"
            );
            Ok(id)
        }
        Err(e) => {
            eprintln!(
                "darkmux crew dispatch: select_model error ({e}); falling back \
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
/// `None` only when the registry/profile can't be resolved — the same edge
/// cases that send model selection to `probe_loaded_model()`.
// `pub` so the mission-run brief path can size its proportional injected-context
// budget (#1011) from the SAME profile resolver the runtime uses for its
// compaction window. They share the resolver; a profile that declares no
// `context_window` falls back independently on each side (the budget to its own
// default), so they agree whenever the profile actually declares a window.
pub fn resolve_context_window_internal(
    profile_override: Option<&str>,
    config_path: Option<&str>,
) -> Option<u32> {
    let loaded = darkmux_profiles::profiles::load_registry(config_path).ok()?;
    // (#1054) Same graceful resolution as model selection — a requested profile
    // undefined here falls back to default_profile, so the context window comes
    // from the SAME profile the model does.
    let (_active_name, profile) = loaded.registry.resolve_active(profile_override)?;
    crate::dispatch::CompactionDispatchArgs::from_profile(profile).context_window
}

/// (#632) Guard that the runtime always receives a context window. Some
/// dispatch paths (bare `crew dispatch`, the lab `prompt` provider) build a
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

/// (#590) Returns a loud warning when the registered utility model isn't in
/// the loaded set — compaction summons it mid-dispatch and a missing one
/// fails at the LMStudio call. `None` ⇒ it's loaded (matched by either the
/// `modelKey` or the namespaced `identifier`). Pure, for testing.
fn utility_preflight_warning(
    util_id: &str,
    loaded: &[darkmux_types::LoadedModel],
) -> Option<String> {
    let is_loaded = loaded
        .iter()
        .any(|m| m.model == util_id || m.identifier == util_id);
    if is_loaded {
        None
    } else {
        Some(format!(
            "darkmux crew dispatch: WARNING — utility model `{util_id}` \
             (internal.utility) is NOT loaded; compaction summons it mid-dispatch \
             and will fail if it isn't resident. Load it now (`lms load {util_id}`, \
             or include it in the profile you `darkmux swap` to). (#590)"
        ))
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
    // shares the base URL with the sprint chat narrator).
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
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Write;
    use tempfile::TempDir;

    // ─── #888: auto-workspace cleanup guard ───────────────────────────

    #[test]
    fn auto_workspace_cleanup_removes_dir_when_armed_on_drop() {
        // Simulates an error/panic exit before the dispatch completed: the
        // armed guard reclaims the auto-allocated scratch workspace.
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("darkmux-dispatch-coder-123");
        std::fs::create_dir_all(workspace.join("subdir")).unwrap();
        std::fs::write(workspace.join("subdir/scratch.txt"), b"agent work").unwrap();
        {
            let _guard = AutoWorkspaceCleanup {
                workspace: Some(workspace.clone()),
                armed: true,
            };
        } // drop here
        assert!(!workspace.exists(), "armed guard must reclaim the workspace on drop");
    }

    #[test]
    fn auto_workspace_cleanup_retains_dir_when_disarmed() {
        // Simulates a completed dispatch: disarmed → workspace retained for
        // inspection (status quo).
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("darkmux-dispatch-coder-456");
        std::fs::create_dir_all(&workspace).unwrap();
        {
            let mut guard = AutoWorkspaceCleanup {
                workspace: Some(workspace.clone()),
                armed: true,
            };
            guard.disarm();
        } // drop here
        assert!(workspace.exists(), "disarmed guard must retain the workspace");
    }

    #[test]
    fn auto_workspace_cleanup_never_touches_operator_workdir() {
        // The `--workdir` case stores `None`, so an armed drop is a no-op —
        // an operator-provided path is never reclaimed by construction.
        let tmp = TempDir::new().unwrap();
        let operator_workdir = tmp.path().join("my-repo");
        std::fs::create_dir_all(&operator_workdir).unwrap();
        {
            let _guard = AutoWorkspaceCleanup {
                workspace: None,
                armed: true,
            };
        } // drop here — must NOT touch anything
        assert!(operator_workdir.exists(), "operator --workdir must never be cleaned");
    }

    // ─── #680: Docker-runtime preflight status → bail mapping ─────────

    #[test]
    fn preflight_ready_is_ok() {
        assert!(preflight_result_for(DockerRuntimeStatus::Ready).is_ok());
    }

    #[test]
    fn preflight_binary_missing_bails_with_install_hint() {
        let msg = preflight_result_for(DockerRuntimeStatus::BinaryMissing)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("isn't on PATH"), "{msg}");
        assert!(msg.contains("Install Docker Desktop"), "{msg}");
    }

    #[test]
    fn preflight_daemon_unreachable_surfaces_stderr_and_start_hint() {
        let msg = preflight_result_for(DockerRuntimeStatus::DaemonUnreachable(
            "Cannot connect to the Docker daemon".to_string(),
        ))
        .unwrap_err()
        .to_string();
        assert!(msg.contains("`docker version` failed"), "{msg}");
        assert!(msg.contains("Start Docker Desktop"), "{msg}");
        assert!(msg.contains("Cannot connect to the Docker daemon"), "{msg}");
    }

    #[test]
    fn preflight_image_missing_mentions_pull_then_build_fallback() {
        // ImageMissing is the fallback mapper message now — the live dispatch
        // path pulls the GHCR image on demand (#759) rather than bailing. The
        // message should name the GHCR pull as primary and build-from-source
        // as the fallback.
        let msg = preflight_result_for(DockerRuntimeStatus::ImageMissing)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("GHCR"), "{msg}");
        assert!(msg.contains("ghcr.io/kstrat2001/darkmux-runtime:"), "{msg}");
        assert!(
            msg.contains("docker build -t darkmux-runtime:latest runtime/"),
            "{msg}"
        );
    }

    #[test]
    fn ghcr_image_pins_to_the_crate_version() {
        // Pull-on-demand pins to the darkmux binary version so a `brew upgrade`
        // fetches the matching image (#759).
        assert_eq!(
            ghcr_runtime_image(),
            format!("ghcr.io/kstrat2001/darkmux-runtime:{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn darkmux_runtime_image_recognized_no_inject() {
        // Both the local dev tag and any GHCR-published tag are darkmux's own
        // images (binary baked in → no injection). An operator `--image` is not.
        assert!(is_darkmux_runtime_image(RUNTIME_IMAGE));
        assert!(is_darkmux_runtime_image(&ghcr_runtime_image()));
        assert!(is_darkmux_runtime_image(
            "ghcr.io/kstrat2001/darkmux-runtime:1.2.3"
        ));
        assert!(!is_darkmux_runtime_image("rust:slim"));
        assert!(!is_darkmux_runtime_image("ubuntu:24.04"));
        // A lookalike repo prefix without the `:tag` separator must not match.
        assert!(!is_darkmux_runtime_image(
            "ghcr.io/kstrat2001/darkmux-runtime-evil:latest"
        ));
    }

    #[test]
    fn preflight_probe_error_bails_with_underlying_error() {
        // The one arm whose behavior changed (old `.context(...)?` → `anyhow!`).
        let msg = preflight_result_for(DockerRuntimeStatus::ProbeError("boom".to_string()))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("running `docker images`"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }

    // ─── #368: compaction-flag passthrough to runtime CLI ────────────

    // ─── out-of-band bookkeeping: volume mounts ──────────────────────

    #[test]
    fn read_token_totals_parses_metrics_json() {
        // metrics.json lives under <out_dir>/.darkmux-runtime/ — the same
        // out-dir the trajectory tailer reads. read_token_totals pulls the
        // runtime's recorded prompt/completion totals; total() is derived.
        let out = TempDir::new().unwrap();
        let rt = out.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(
            rt.join("metrics.json"),
            r#"{"total_prompt_tokens": 1200, "total_completion_tokens": 345, "turns": 4}"#,
        )
        .unwrap();
        let t = read_token_totals(out.path());
        assert_eq!(t.prompt, 1200);
        assert_eq!(t.completion, 345);
        assert_eq!(t.total(), 1545);
    }

    #[test]
    fn read_token_totals_degrades_to_zero_on_missing_or_malformed() {
        // Observability enrichment, never a dispatch-failing path: a missing
        // file (container died before writing) or malformed JSON yields zero
        // totals rather than erroring.
        let missing = TempDir::new().unwrap();
        let t = read_token_totals(missing.path());
        assert_eq!(t.total(), 0, "missing metrics.json → zero totals");

        let bad = TempDir::new().unwrap();
        let rt = bad.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(rt.join("metrics.json"), "{not valid json").unwrap();
        let t = read_token_totals(bad.path());
        assert_eq!(t.total(), 0, "malformed metrics.json → zero totals");
    }

    #[test]
    fn token_totals_total_saturates() {
        // Guard the derived sum against overflow on absurd inputs (the
        // runtime caps real totals far below this, but the helper must not
        // panic in a release build with overflow checks off — saturate).
        let t = TokenTotals { prompt: u32::MAX, completion: 10 };
        assert_eq!(t.total(), u32::MAX);
    }

    #[test]
    #[serial]
    fn config_path_reaches_dispatch_resolvers_not_just_env() {
        // (#984) Regression: the dispatch's resolvers called `load_registry(None)`,
        // so a lab `--profiles-file` silently used the default registry's model —
        // the flag reached lab run's own lookup but never the dispatch. With
        // `config_path` threaded, a resolver loads from the passed file even with
        // NO `DARKMUX_PROFILES` env. `resolve_context_window_internal` is the
        // simplest of the three resolvers to exercise; the model + utility
        // resolvers thread `config_path` identically. Fails on the old
        // `load_registry(None)`; passes on `load_registry(config_path)`.
        let tmp = TempDir::new().unwrap();
        let pf = tmp.path().join("profiles.json");
        std::fs::write(
            &pf,
            r#"{"profiles":{"probe":{"models":[{"id":"probe-model","n_ctx":12345,"role":"primary"}],"runtime":{"compaction":{"mode":"default"}}}},"default_profile":"probe"}"#,
        )
        .unwrap();
        // Clear the env so this proves the FLAG (config_path), not the env workaround.
        let prev = std::env::var("DARKMUX_PROFILES").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::remove_var("DARKMUX_PROFILES") };
        let from_flag = resolve_context_window_internal(None, pf.to_str());
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_PROFILES", v),
                None => std::env::remove_var("DARKMUX_PROFILES"),
            }
        }
        assert_eq!(
            from_flag,
            Some(12345),
            "config_path (lab --profiles-file) must reach the dispatch resolver, \
             not fall through to env/default (#984)"
        );
    }

    #[test]
    fn apply_volume_mounts_emits_workspace_and_out_dir() {
        // The runtime's OWN bookkeeping goes to `/darkmux-out`, SEPARATE
        // from the agent's `/workspace`. This literal MUST stay in sync
        // with `runtime::trajectory::RUNTIME_OUT_BASE` (the two crates
        // can't share the const — the runtime is built into the image).
        let mut args: Vec<String> = Vec::new();
        apply_volume_mounts(
            &mut args,
            Path::new("/host/workspace"),
            Path::new("/host/out"),
        );
        assert_eq!(
            args,
            vec![
                "-v",
                "/host/workspace:/workspace",
                "-v",
                "/host/out:/darkmux-out",
            ],
            "both binds present; out-dir mounts at /darkmux-out"
        );
        // Defensive: the runtime must NOT be told to write its
        // bookkeeping into the workspace tree.
        assert!(
            !args
                .iter()
                .any(|a| a.ends_with(":/workspace") && a.contains("/host/out")),
            "out-dir must not be mounted at /workspace"
        );
    }

    #[test]
    fn apply_runtime_injection_mounts_binary_and_overrides_entrypoint() {
        // (#703) Injecting into a non-default image: bind the static binary
        // read-only at /darkmux-runtime and force the entrypoint to it. These
        // are docker-run OPTIONS (must precede the image arg).
        let mut args: Vec<String> = Vec::new();
        apply_runtime_injection(&mut args, Path::new("/home/op/.darkmux/runtime/darkmux-runtime"));
        assert_eq!(
            args,
            vec![
                "-v",
                "/home/op/.darkmux/runtime/darkmux-runtime:/darkmux-runtime:ro",
                "--entrypoint",
                "/darkmux-runtime",
            ],
            "binary bound read-only at /darkmux-runtime; entrypoint overridden to it"
        );
    }

    #[test]
    fn apply_cache_mount_binds_cache_and_points_package_managers_at_it() {
        // (#703 Slice 3) Shared toolchain cache mounted at /darkmux-cache with
        // CARGO_HOME / npm / pip env redirected so the inner loop reuses
        // downloads across dispatches.
        let mut args: Vec<String> = Vec::new();
        apply_cache_mount(&mut args, Path::new("/home/op/.darkmux/cache"));
        assert_eq!(
            args,
            vec![
                "-v",
                "/home/op/.darkmux/cache:/darkmux-cache",
                "-e",
                "CARGO_HOME=/darkmux-cache/cargo",
                "-e",
                "npm_config_cache=/darkmux-cache/npm",
                "-e",
                "PIP_CACHE_DIR=/darkmux-cache/pip",
            ],
            "cache bound at /darkmux-cache; cargo/npm/pip caches redirected into it"
        );
    }

    // ─── #839 + #842: full docker-run argv assertion ──────────────

    #[test]
    fn build_docker_run_argv_asserts_complete_vector() {
        // (#842) Representative dispatch: non-default image (inject=true),
        // with compaction, allowed tools, and json mode. Asserts the
        // COMPLETE argv vector including all hardening flags (#839).
        let config = DockerRunConfig {
            output_schema: None,
            container_name: "darkmux-dispatch-test-123".to_string(),
            workspace: PathBuf::from("/host/workspace"),
            host_out: PathBuf::from("/host/out"),
            inject: true,
            runtime_binary: Some(PathBuf::from(
                "/home/op/.darkmux/runtime/darkmux-runtime",
            )),
            image: "rust:slim".to_string(),
            model: "llama3-8b".to_string(),
            system_prompt: "You are a coding assistant.".to_string(),
            message: "Fix the bug in main.rs".to_string(),
            json: true,
            allowed_tools: Some(vec!["exec".to_string(), "edit".to_string()]),
            compaction: crate::dispatch::CompactionDispatchArgs {
                threshold_tokens: Some(4096),
                compactor_model: Some("util-model".to_string()),
                threshold_ratio: Some(0.75),
                context_window: Some(32000),
                // All six compaction flags set so each emission is pinned —
                // the regression that shipped a wrong flag name + dropped
                // three of these (strategy/bail/custom) is exactly what an
                // all-fields-set assertion catches.
                strategy: Some(darkmux_types::CompactionStrategy::StructuredSlot),
                bail_after_compactions: Some(10u32),
                custom_instructions: Some("Be terse.".to_string()),
            },
            feedback_templates: serde_json::json!({
                "error": "An error occurred."
            }),
            cache_dir: PathBuf::from("/home/op/.darkmux/cache"),
        };

        let argv = build_docker_run_argv(&config);

        // 1. Verify the hardening flags are present (#839)
        assert!(
            argv.contains(&format!("--cap-drop={}", DOCKER_CAP_DROP)),
            "must include --cap-drop ALL"
        );
        assert!(
            argv.contains(&format!("--security-opt={}", DOCKER_SECURITY_OPT)),
            "must include --security-opt no-new-privileges"
        );
        assert!(
            argv.contains(&format!("--pids-limit={}", DOCKER_PIDS_LIMIT)),
            "must include --pids-limit 512"
        );
        assert!(
            argv.contains(&format!("--memory={}", DOCKER_MEMORY)),
            "must include --memory 4g"
        );

        // 2. Verify the full argv structure
        assert_eq!(argv[0], "docker");
        assert_eq!(argv[1], "run");
        assert_eq!(argv[2], "--rm");
        assert_eq!(argv[3], "--name");
        assert_eq!(argv[4], "darkmux-dispatch-test-123");

        // 3. Verify hardening flags follow immediately after --name
        assert_eq!(argv[5], format!("--cap-drop={}", DOCKER_CAP_DROP));
        assert_eq!(argv[6], format!("--security-opt={}", DOCKER_SECURITY_OPT));
        assert_eq!(argv[7], format!("--pids-limit={}", DOCKER_PIDS_LIMIT));
        assert_eq!(argv[8], format!("--memory={}", DOCKER_MEMORY));

        // 4. Verify volume mounts (workspace + out-dir)
        assert_eq!(argv[9], "-v");
        assert_eq!(argv[10], "/host/workspace:/workspace");
        assert_eq!(argv[11], "-v");
        assert_eq!(argv[12], "/host/out:/darkmux-out");

        // 5. Verify cache mount + env vars (real host:container bind, not a
        // bare anonymous volume).
        assert_eq!(argv[13], "-v");
        assert_eq!(argv[14], "/home/op/.darkmux/cache:/darkmux-cache");
        assert_eq!(argv[15], "-e");
        assert_eq!(argv[16], "CARGO_HOME=/darkmux-cache/cargo");
        assert_eq!(argv[17], "-e");
        assert_eq!(argv[18], "npm_config_cache=/darkmux-cache/npm");
        assert_eq!(argv[19], "-e");
        assert_eq!(argv[20], "PIP_CACHE_DIR=/darkmux-cache/pip");

        // 6. Verify runtime injection (non-default image)
        assert_eq!(argv[21], "-v");
        assert_eq!(
            argv[22],
            "/home/op/.darkmux/runtime/darkmux-runtime:/darkmux-runtime:ro"
        );
        assert_eq!(argv[23], "--entrypoint");
        assert_eq!(argv[24], "/darkmux-runtime");

        // 7. Verify `--` + image + runtime CLI args
        assert_eq!(argv[25], "--");
        assert_eq!(argv[26], "rust:slim"); // image
        assert_eq!(argv[27], "run"); // runtime subcommand
        assert_eq!(argv[28], "--model");
        assert_eq!(argv[29], "llama3-8b");
        assert_eq!(argv[30], "--system");
        assert_eq!(argv[31], "You are a coding assistant.");
        // (#386) The message goes via the out-dir mount, not argv — argv carries
        // the constant `--prompt-file <container path>`, never the brief itself.
        assert_eq!(argv[32], "--prompt-file");
        assert_eq!(argv[33], "/darkmux-out/.prompt.txt");
        assert!(
            !argv.iter().any(|a| a == "Fix the bug in main.rs"),
            "the message must NOT appear anywhere in the docker argv (#386): {argv:?}"
        );

        // 8. Verify json flag
        assert_eq!(argv[34], "--json");

        // 9. Verify allowed tools
        assert_eq!(argv[35], "--allowed-tools");
        assert_eq!(argv[36], "exec,edit");

        // 10. Verify compaction flags — flag names must match the runtime's
        // accepted set verbatim (an unknown flag exits the container with 2).
        assert_eq!(argv[37], "--compact-threshold-tokens");
        assert_eq!(argv[38], "4096");
        assert_eq!(argv[39], "--compactor-model");
        assert_eq!(argv[40], "util-model");
        assert_eq!(argv[41], "--compact-threshold-ratio");
        assert_eq!(argv[42], "0.75");
        assert_eq!(argv[43], "--context-window");
        assert_eq!(argv[44], "32000");
        assert_eq!(argv[45], "--compact-strategy");
        assert_eq!(argv[46], "structured-slot");
        assert_eq!(argv[47], "--bail-after-compactions");
        assert_eq!(argv[48], "10");
        assert_eq!(argv[49], "--compactor-custom-instructions");
        assert_eq!(argv[50], "Be terse.");

        // 11. Verify feedback templates JSON
        assert_eq!(argv[51], "--feedback-templates-json");
        // The JSON value should contain the error template
        assert!(argv[52].contains("error"));
        assert!(argv[52].contains("An error occurred"));

        // Total arg count: 53 (0..=52)
        assert_eq!(argv.len(), 53);
    }

    #[test]
    fn build_docker_run_argv_minimal_dispatch_no_injection() {
        // Minimal dispatch: default darkmux image (inject=false), no
        // compaction, no allowed tools, no json — asserts that optional
        // flags are omitted when not set.
        let config = DockerRunConfig {
            output_schema: None,
            container_name: "darkmux-dispatch-min".to_string(),
            workspace: PathBuf::from("/tmp/ws"),
            host_out: PathBuf::from("/tmp/out"),
            inject: false,
            runtime_binary: None,
            image: "darkmux-runtime:latest".to_string(),
            model: "default-model".to_string(),
            system_prompt: "Basic role.".to_string(),
            message: "Hello world".to_string(),
            json: false,
            allowed_tools: None,
            compaction: crate::dispatch::CompactionDispatchArgs::default(),
            feedback_templates: serde_json::Value::Null,
            cache_dir: PathBuf::from("/tmp/cache"),
        };

        let argv = build_docker_run_argv(&config);

        // Should NOT contain optional flags
        assert!(
            !argv.contains(&"--json".to_string()),
            "minimal dispatch should NOT have --json"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("--allowed-tools")),
            "minimal dispatch should NOT have --allowed-tools"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("--compact")),
            "minimal dispatch should NOT have --compact-* flags"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("--compactor-model")),
            "minimal dispatch should NOT have --compactor-model"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("--context-window")),
            "minimal dispatch should NOT have --context-window"
        );

        // Should still contain hardening flags
        assert!(argv.contains(&format!("--cap-drop={}", DOCKER_CAP_DROP)));
        assert!(argv.contains(&format!("--security-opt={}", DOCKER_SECURITY_OPT)));
        assert!(argv.contains(&format!("--pids-limit={}", DOCKER_PIDS_LIMIT)));
        assert!(argv.contains(&format!("--memory={}", DOCKER_MEMORY)));

        // Should NOT contain injection args (no binary mount/entrypoint)
        assert!(
            !argv.iter().any(|a| a.contains("/darkmux-runtime:ro")),
            "minimal dispatch should NOT have runtime binary mount"
        );

        // Verify image and model are present after --
        let dash_idx = argv.iter().position(|a| *a == "--").unwrap();
        assert_eq!(argv[dash_idx + 1], "darkmux-runtime:latest");
        assert_eq!(argv[dash_idx + 2], "run");

        // (#1038) No output_schema ⇒ no --response-schema flag.
        assert!(
            !argv.iter().any(|a| a == "--response-schema"),
            "absent output_schema must NOT emit --response-schema"
        );
    }

    #[test]
    fn build_docker_run_argv_output_schema_emits_response_schema_flag() {
        // (#1038) The grammar-constrained-output branch: a Some(output_schema)
        // must serialize to a `--response-schema <json>` flag pair in the argv.
        // The whole feature rides on this flag reaching the runtime — exercise
        // the live branch, not just the absent case (the #975 lesson: assert
        // the real construction, not only the omit path).
        let schema = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "verdict": { "type": "string" } },
            "required": ["verdict"]
        });
        let config = DockerRunConfig {
            output_schema: Some(schema.clone()),
            container_name: "darkmux-dispatch-schema".to_string(),
            workspace: PathBuf::from("/tmp/ws"),
            host_out: PathBuf::from("/tmp/out"),
            inject: false,
            runtime_binary: None,
            image: "darkmux-runtime:latest".to_string(),
            model: "default-model".to_string(),
            system_prompt: "Tool-less reviewer.".to_string(),
            message: "Review this.".to_string(),
            json: true,
            allowed_tools: None,
            compaction: crate::dispatch::CompactionDispatchArgs::default(),
            feedback_templates: serde_json::Value::Null,
            cache_dir: PathBuf::from("/tmp/cache"),
        };

        let argv = build_docker_run_argv(&config);

        let idx = argv
            .iter()
            .position(|a| a == "--response-schema")
            .expect("Some(output_schema) must emit --response-schema");
        // The flag's value is the schema serialized as a single JSON string,
        // and it must round-trip back to the exact schema (no corruption).
        let value = &argv[idx + 1];
        let parsed: serde_json::Value =
            serde_json::from_str(value).expect("--response-schema value must be valid JSON");
        assert_eq!(parsed, schema, "schema must round-trip through argv intact");
    }

    // ─── #842 edge cases the complete-vector test doesn't exercise ──────
    // The complete-vector test only ever runs StructuredSlot + a non-empty
    // allowed-tools vec + a non-empty feedback object. These pin the three
    // remaining branches: the Narrative→kebab mapping, the
    // Some(empty)-vs-None allowed-tools fork (block-all vs allow-all), and
    // the empty-feedback-object guard.

    /// A minimal valid config: no optional flags set. Each test below flips
    /// exactly one field so the assertion isolates that branch.
    fn base_argv_config() -> DockerRunConfig {
        DockerRunConfig {
            output_schema: None,
            container_name: "darkmux-edge".to_string(),
            workspace: PathBuf::from("/tmp/ws"),
            host_out: PathBuf::from("/tmp/out"),
            inject: false,
            runtime_binary: None,
            image: "darkmux-runtime:latest".to_string(),
            model: "m".to_string(),
            system_prompt: "role".to_string(),
            message: "msg".to_string(),
            json: false,
            allowed_tools: None,
            compaction: crate::dispatch::CompactionDispatchArgs::default(),
            feedback_templates: serde_json::Value::Null,
            cache_dir: PathBuf::from("/tmp/cache"),
        }
    }

    #[test]
    fn build_docker_run_argv_compaction_strategy_narrative_is_kebab() {
        // Only StructuredSlot is exercised by the complete-vector test; a typo
        // in the Narrative arm (line ~333) would ship green. The runtime
        // rejects an unknown flag value, so the kebab string must be exact.
        let mut config = base_argv_config();
        config.compaction.strategy = Some(darkmux_types::CompactionStrategy::Narrative);
        let argv = build_docker_run_argv(&config);
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--compact-strategy" && w[1] == "narrative"),
            "Narrative must map to the kebab `narrative`, got: {argv:?}"
        );
        // And NOT the Debug-derived PascalCase, which the runtime would reject.
        assert!(
            !argv.iter().any(|a| a == "Narrative"),
            "must not emit the enum's Debug form: {argv:?}"
        );
    }

    #[test]
    fn build_docker_run_argv_empty_allowed_tools_is_block_all_not_omitted() {
        // Some(vec![]) and None are DIFFERENT contracts: Some(empty) emits
        // `--allowed-tools ""` (block-all — a sandbox-bounding semantic), None
        // omits the flag (full catalog). A bug collapsing them is sandbox-
        // adjacent, so pin that the two configs produce different argv.
        let mut empty = base_argv_config();
        empty.allowed_tools = Some(vec![]);
        let empty_argv = build_docker_run_argv(&empty);
        let pos = empty_argv
            .iter()
            .position(|a| a == "--allowed-tools")
            .expect("Some(empty) must still emit --allowed-tools");
        assert_eq!(
            empty_argv[pos + 1], "",
            "block-all is the empty CSV, got: {:?}",
            empty_argv[pos + 1]
        );

        let none_argv = build_docker_run_argv(&base_argv_config());
        assert!(
            !none_argv.iter().any(|a| a == "--allowed-tools"),
            "None must omit the flag entirely (allow-all): {none_argv:?}"
        );
    }

    #[test]
    fn build_docker_run_argv_empty_feedback_object_omits_flag() {
        // The guard is `as_object().is_some_and(|o| !o.is_empty())`. An empty
        // object must NOT emit the flag (it would be a useless empty payload);
        // a non-empty object must. Pin both sides of the guard.
        let mut empty = base_argv_config();
        empty.feedback_templates = serde_json::json!({});
        assert!(
            !build_docker_run_argv(&empty)
                .iter()
                .any(|a| a == "--feedback-templates-json"),
            "empty feedback object must omit the flag"
        );

        let mut filled = base_argv_config();
        filled.feedback_templates = serde_json::json!({ "cycle": "regroup" });
        assert!(
            build_docker_run_argv(&filled)
                .iter()
                .any(|a| a == "--feedback-templates-json"),
            "non-empty feedback object must emit the flag"
        );
    }

    #[test]
    fn docker_command_from_argv_uses_argv0_as_program_not_an_arg() {
        // (#975) Regression: the consumer must build the Command as
        // program=argv[0] + args=argv[1..], NOT push the whole vector. Pushing
        // argv[0] ("docker") as an argument ran `docker docker run …`, which
        // docker rejected with exit 125 — the core internal-runtime dispatch was
        // dead in 1.3.x + 1.4.0. This is the missing test layer #842 named: it
        // inspects the REAL Command the dispatch executes, not just
        // build_docker_run_argv's output vector (which the other tests cover).
        let config = DockerRunConfig {
            output_schema: None,
            container_name: "darkmux-dispatch-reg".to_string(),
            workspace: PathBuf::from("/tmp/ws"),
            host_out: PathBuf::from("/tmp/out"),
            inject: false,
            runtime_binary: None,
            image: "darkmux-runtime:latest".to_string(),
            model: "default-model".to_string(),
            system_prompt: "Basic role.".to_string(),
            message: "Hello world".to_string(),
            json: false,
            allowed_tools: None,
            compaction: crate::dispatch::CompactionDispatchArgs::default(),
            feedback_templates: serde_json::Value::Null,
            cache_dir: PathBuf::from("/tmp/cache"),
        };
        let argv = build_docker_run_argv(&config);
        let cmd = docker_command_from_argv(&argv);

        // Program is `docker`, and the arguments start at `run` — never a
        // spurious leading `docker`.
        assert_eq!(cmd.get_program().to_str().unwrap(), "docker");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args.first().map(String::as_str),
            Some("run"),
            "args must start at `run`, not a second `docker`: {args:?}"
        );
        assert_eq!(args.get(1).map(String::as_str), Some("--rm"));
        assert!(
            !args.contains(&"docker".to_string()),
            "no spurious `docker` in the arguments (the #975 `docker docker run` bug): {args:?}"
        );
    }

    // ─── #408: strict-selection opt-in parsing ───────────────────────

    #[serial]
    #[test]
    fn strict_selection_enabled_reads_env_truthy_values() {
        let prev = std::env::var("DARKMUX_STRICT_SELECTION").ok();

        unsafe { std::env::remove_var("DARKMUX_STRICT_SELECTION"); }
        assert!(!strict_selection_enabled(), "unset ⇒ off (back-compat default)");

        for truthy in ["1", "true", "TRUE", "Yes", " on "] {
            unsafe { std::env::set_var("DARKMUX_STRICT_SELECTION", truthy); }
            assert!(strict_selection_enabled(), "`{truthy}` should enable strict mode");
        }
        for falsy in ["0", "false", "no", "off", ""] {
            unsafe { std::env::set_var("DARKMUX_STRICT_SELECTION", falsy); }
            assert!(!strict_selection_enabled(), "`{falsy}` should NOT enable strict mode");
        }

        match prev {
            Some(v) => unsafe { std::env::set_var("DARKMUX_STRICT_SELECTION", v) },
            None => unsafe { std::env::remove_var("DARKMUX_STRICT_SELECTION") },
        }
    }

    #[test]
    fn apply_compaction_flags_omits_when_all_none() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs::default();
        apply_compaction_flags(&mut args, &compaction);
        assert!(
            !args.iter().any(|a| a.starts_with("--compact") || a == "--context-window"),
            "default config should emit no compaction flags; got {args:?}"
        );
    }

    #[test]
    fn apply_compaction_flags_emits_threshold_when_set() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs {
            threshold_tokens: Some(35_000),
            ..Default::default()
        };
        apply_compaction_flags(&mut args, &compaction);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--compact-threshold-tokens" && w[1] == "35000"),
            "expected --compact-threshold-tokens 35000; got {args:?}"
        );
    }

    #[test]
    fn ensure_context_window_fills_when_none() {
        let mut c = crate::dispatch::CompactionDispatchArgs::default();
        assert_eq!(c.context_window, None);
        ensure_context_window(&mut c, Some(101_000));
        assert_eq!(c.context_window, Some(101_000));
    }

    #[test]
    fn ensure_context_window_preserves_existing() {
        let mut c = crate::dispatch::CompactionDispatchArgs {
            context_window: Some(50_000),
            ..Default::default()
        };
        ensure_context_window(&mut c, Some(101_000));
        assert_eq!(
            c.context_window,
            Some(50_000),
            "an already-set window must win over the fallback"
        );
    }

    #[test]
    fn ensure_context_window_stays_none_without_fallback() {
        let mut c = crate::dispatch::CompactionDispatchArgs::default();
        ensure_context_window(&mut c, None);
        assert_eq!(c.context_window, None);
    }

    // (#632 regression) A `default()` compaction — what bare `crew dispatch`
    // and the lab `prompt` provider build — must emit `--context-window`
    // once the guard fills it, so the runtime can derive its compaction
    // threshold instead of hard-erroring.
    #[test]
    fn ensure_context_window_then_apply_emits_flag() {
        let mut args: Vec<String> = Vec::new();
        let mut compaction = crate::dispatch::CompactionDispatchArgs::default();
        ensure_context_window(&mut compaction, Some(262_144));
        apply_compaction_flags(&mut args, &compaction);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--context-window" && w[1] == "262144"),
            "expected --context-window 262144 after the guard; got {args:?}"
        );
    }

    #[test]
    fn apply_compaction_flags_emits_all_when_set() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs {
            threshold_tokens: Some(45_000),
            compactor_model: Some("custom-compactor".to_string()),
            threshold_ratio: Some(0.35),
            context_window: Some(101_000),
            strategy: Some(darkmux_types::CompactionStrategy::StructuredSlot),
            bail_after_compactions: None,
            custom_instructions: None,
        };
        apply_compaction_flags(&mut args, &compaction);
        assert!(args.iter().any(|a| a == "--compact-threshold-tokens"));
        assert!(args.iter().any(|a| a == "45000"));
        assert!(args.iter().any(|a| a == "--compactor-model"));
        assert!(args.iter().any(|a| a == "custom-compactor"));
        assert!(args.iter().any(|a| a == "--compact-threshold-ratio"));
        assert!(args.iter().any(|a| a == "--context-window"));
        assert!(args.iter().any(|a| a == "101000"));
        assert!(args.iter().any(|a| a == "--compact-strategy"));
        assert!(args.iter().any(|a| a == "structured-slot"));
    }

    /// (#377) Escalation bound emits `--bail-after-compactions N`
    /// when set; omitted when None (back-compat with pre-#377 runtime
    /// + back-compat with operators who haven't configured the bound).
    #[test]
    fn apply_compaction_flags_emits_bail_when_set() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs {
            bail_after_compactions: Some(3),
            custom_instructions: None,
            ..Default::default()
        };
        apply_compaction_flags(&mut args, &compaction);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--bail-after-compactions" && w[1] == "3"),
            "expected --bail-after-compactions 3; got {args:?}"
        );
    }

    #[test]
    fn apply_compaction_flags_omits_bail_when_none() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs::default();
        apply_compaction_flags(&mut args, &compaction);
        assert!(
            !args.iter().any(|a| a == "--bail-after-compactions"),
            "no bail flag should appear when bail_after_compactions is None; got {args:?}"
        );
    }

    /// (#383) Custom instructions emit `--compactor-custom-instructions
    /// <text>` when set; omitted when None. Schema-isolation contract:
    /// the typed `profile.runtime.compaction.custom_instructions` is
    /// the only source the runtime sees — extras["customInstructions"]
    /// is dead-letter (handled by the `from_profile_ignores_extras_*`
    /// tests below).
    #[test]
    fn apply_compaction_flags_emits_custom_instructions_when_set() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs {
            custom_instructions: Some(
                "Preserve verbatim X / list active files with what was learned".into(),
            ),
            ..Default::default()
        };
        apply_compaction_flags(&mut args, &compaction);
        assert!(
            args.windows(2).any(|w| w[0] == "--compactor-custom-instructions"
                && w[1] == "Preserve verbatim X / list active files with what was learned"),
            "expected --compactor-custom-instructions with operator text; got {args:?}"
        );
    }

    #[test]
    fn apply_compaction_flags_omits_custom_instructions_when_none() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs::default();
        apply_compaction_flags(&mut args, &compaction);
        assert!(
            !args.iter().any(|a| a == "--compactor-custom-instructions"),
            "no custom-instructions flag should appear when None; got {args:?}"
        );
    }

    /// (#372 T2-C) Strategy alone (no other overrides) still emits
    /// just `--compact-strategy <kebab>` so the runtime can pick up
    /// the operator's tier-2 opt-in without requiring the operator
    /// to also override threshold/model/etc.
    #[test]
    fn apply_compaction_flags_strategy_only_emits_just_strategy_flag() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs {
            strategy: Some(darkmux_types::CompactionStrategy::StructuredSlot),
            ..Default::default()
        };
        apply_compaction_flags(&mut args, &compaction);
        assert!(args.windows(2).any(|w| w[0] == "--compact-strategy" && w[1] == "structured-slot"));
        // Only the strategy flag should be present.
        assert!(!args.iter().any(|a| a == "--compact-threshold-tokens"));
        assert!(!args.iter().any(|a| a == "--context-window"));
    }

    #[test]
    fn from_profile_reads_typed_strategy_field() {
        use darkmux_types::{
            CompactionStrategy, Profile, ProfileModel, ProfileRuntime,
            RuntimeCompactionConfig,
        };
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                extras: Default::default(),
                id: "primary".into(),
                n_ctx: 100_000,
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                config_path: None,
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: Some(CompactionStrategy::StructuredSlot),
                    threshold_tokens: None,
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    custom_instructions: None,
                    extras: Default::default(),
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(args.strategy, Some(CompactionStrategy::StructuredSlot));
    }

    /// (#377) `from_profile` reads `compaction.reserve.bail_after_compactions`
    /// (typed field that landed in #357) and surfaces it on
    /// `CompactionDispatchArgs` so apply_compaction_flags can plumb the
    /// `--bail-after-compactions N` CLI flag to the runtime. Profile-
    /// level only at this chunk; per-role override comes in chunk 4.
    #[test]
    fn from_profile_derives_bail_after_compactions_from_reserve() {
        use darkmux_types::{
            Profile, ProfileModel, ProfileRuntime, ReserveConfig,
            RuntimeCompactionConfig,
        };
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: 100_000,
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                config_path: None,
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: None,
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: Some(ReserveConfig {
                        bail_after_token_count: None,
                        bail_after_compactions: Some(3),
                    }),
                    custom_instructions: None,
                    extras: Default::default(),
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(args.bail_after_compactions, Some(3));
    }

    /// (#377) Per-role override wins over profile fallback. Operator
    /// pins `bail_after_compactions = 2` on the coder role; profile
    /// default is 5. Resolved value must be the role's 2, NOT the
    /// profile's 5.
    #[test]
    fn apply_role_override_overlays_role_bail_on_top_of_profile() {
        use crate::dispatch::CompactionDispatchArgs;
        use crate::types::{EscalationContract, Role, ToolPalette};
        let mut args = CompactionDispatchArgs {
            bail_after_compactions: Some(5), // profile default
            ..Default::default()
        };
        let role = Role {
            output_schema: None,
            id: "coder".into(),
            description: "test".into(),
            skills: vec![],
            tool_palette: ToolPalette::default(),
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: Some(2), // role pin
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        };
        args.apply_role_override(&role);
        assert_eq!(args.bail_after_compactions, Some(2), "role pin wins");
    }

    /// (#377) When the role's `bail_after_compactions` is None, the
    /// profile fallback survives. Catches the regression where
    /// apply_role_override unconditionally writes the field (would
    /// clobber profile defaults to None for roles that haven't opted
    /// into per-role escalation pinning).
    #[test]
    fn apply_role_override_preserves_profile_default_when_role_unset() {
        use crate::dispatch::CompactionDispatchArgs;
        use crate::types::{EscalationContract, Role, ToolPalette};
        let mut args = CompactionDispatchArgs {
            bail_after_compactions: Some(5), // profile default
            ..Default::default()
        };
        let role = Role {
            output_schema: None,
            id: "coder".into(),
            description: "test".into(),
            skills: vec![],
            tool_palette: ToolPalette::default(),
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None, // role didn't pin
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        };
        args.apply_role_override(&role);
        assert_eq!(
            args.bail_after_compactions,
            Some(5),
            "profile fallback survives when role doesn't pin"
        );
    }

    #[test]
    fn from_profile_bail_after_compactions_is_none_when_reserve_absent() {
        use darkmux_types::{
            Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: 100_000,
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                config_path: None,
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: None,
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    custom_instructions: None,
                    extras: Default::default(),
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(args.bail_after_compactions, None);
    }

    #[test]
    fn from_profile_derives_typed_threshold() {
        use darkmux_types::{
            Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: 100_000,
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                config_path: None,
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: Some(40_000),
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    custom_instructions: None,
                    extras: Default::default(),
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(args.threshold_tokens, Some(40_000));
        assert_eq!(args.context_window, Some(100_000), "primary n_ctx");
    }

    #[test]
    fn from_profile_derives_typed_threshold_ratio() {
        // (#368 clean break) `threshold_ratio` reads ONLY from the
        // typed schema field. `compactor_model` does NOT read from
        // extras at all (Beat-39 smoke caught HTTP 400 when openclaw's
        // `lmstudio/<id>` format was passed to LMStudio's direct API
        // which only knows the bare/namespaced form). Until a typed
        // `compaction.compactor_model` lands, runtime uses default.
        use darkmux_types::{
            Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let mut extras = serde_json::Map::new();
        // This openclaw-flavored value must NOT influence the dispatch.
        extras.insert(
            "model".into(),
            serde_json::json!("lmstudio/qwen3-4b-instruct-2507"),
        );
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: 101_000,
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                config_path: None,
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: None,
                    threshold_ratio: Some(0.35),
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    custom_instructions: None,
                    extras,
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(args.threshold_ratio, Some(0.35));
        assert!(
            args.compactor_model.is_none(),
            "clean break: openclaw extras `model` must NOT auto-populate compactor_model \
             (would pass `lmstudio/<id>` prefix to LMStudio's direct API → HTTP 400)"
        );
        assert_eq!(args.context_window, Some(101_000));
    }

    /// (#368 clean break invariant) When ONLY `extras["maxHistoryShare"]`
    /// is set — no typed `threshold_ratio` — the host MUST NOT silently
    /// translate openclaw's history-cap to darkmux's pre-trigger ratio.
    /// They're different concepts; mapping across would surface in
    /// methodology citations as "this run tuned to X" when the operator
    /// never actually expressed X in the darkmux-side surface.
    #[test]
    fn from_profile_ignores_openclaw_maxhistoryshare_extras() {
        use darkmux_types::{
            Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let mut extras = serde_json::Map::new();
        // Operator carries openclaw's historical config — this should
        // pass through untouched in extras (for any downstream
        // openclaw-aware consumer) but NOT influence darkmux's trigger.
        extras.insert("maxHistoryShare".into(), serde_json::json!(0.35));
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: 100_000,
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                config_path: None,
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: None,
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    custom_instructions: None,
                    extras,
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert!(
            args.threshold_ratio.is_none(),
            "clean break: openclaw extras must NOT auto-populate threshold_ratio"
        );
    }

    /// (#383) `from_profile` reads the typed `custom_instructions`
    /// field. Schema-isolation invariant: typed field is the only
    /// source the internal runtime sees.
    #[test]
    fn from_profile_reads_typed_custom_instructions() {
        use darkmux_types::{
            Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: 100_000,
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                config_path: None,
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: None,
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    custom_instructions: Some(
                        "Preserve verbatim X / list active files".into(),
                    ),
                    extras: Default::default(),
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(
            args.custom_instructions.as_deref(),
            Some("Preserve verbatim X / list active files")
        );
    }

    /// (#383) `from_profile` IGNORES the openclaw-shape
    /// `extras["customInstructions"]` passthrough — schema-isolation
    /// doctrine (DESIGN.md "Schema isolation: each runtime owns its
    /// own config"). Operators on legacy profiles need to migrate to
    /// the typed `custom_instructions` field; a follow-up under [#380](https://github.com/kstrat2001/darkmux/issues/380) surfaces them via
    /// doctor warning.
    #[test]
    fn from_profile_ignores_openclaw_custom_instructions_extras() {
        use darkmux_types::{
            Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let mut extras = serde_json::Map::new();
        // Operator carries an openclaw-era `customInstructions` string
        // in their profile (the heuristic used to write this; will
        // stop in a follow-up under #380). The internal runtime MUST NOT pick it up — the
        // typed field is the only valid source.
        extras.insert(
            "customInstructions".into(),
            serde_json::json!("openclaw-shape passthrough — must be ignored by internal runtime"),
        );
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: 100_000,
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                config_path: None,
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: None,
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    custom_instructions: None,
                    extras,
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert!(
            args.custom_instructions.is_none(),
            "schema isolation: openclaw extras[customInstructions] must NOT auto-populate typed custom_instructions"
        );
    }

    #[test]
    fn from_profile_handles_missing_compaction_block() {
        use darkmux_types::{Profile, ProfileModel};
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: 50_000,
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: None,
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert!(args.threshold_tokens.is_none());
        assert!(args.compactor_model.is_none());
        assert!(args.threshold_ratio.is_none());
        // Primary n_ctx still captured even without compaction block.
        assert_eq!(args.context_window, Some(50_000));
    }

    // ─── #363, #457: inactivity timeout (formerly wall-clock deadline) ─

    #[test]
    #[serial]
    fn inactivity_timeout_defaults_when_env_unset() {
        // Saved + restored — tests share process env, so be polite.
        let prev = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
        unsafe { std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS") };
        assert_eq!(inactivity_timeout_seconds(), 600); // the config_access default
        if let Some(v) = prev {
            unsafe { std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v) };
        }
    }

    #[test]
    #[serial]
    fn inactivity_timeout_reads_env_override() {
        let prev = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
        unsafe { std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", "30") };
        assert_eq!(inactivity_timeout_seconds(), 30);
        unsafe { std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS") };
        if let Some(v) = prev {
            unsafe { std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v) };
        }
    }

    #[test]
    #[serial]
    fn inactivity_timeout_falls_back_on_garbage_env() {
        let prev = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
        unsafe { std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", "not-a-number") };
        assert_eq!(inactivity_timeout_seconds(), 600); // the config_access default
        unsafe { std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS") };
        if let Some(v) = prev {
            unsafe { std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v) };
        }
    }

    /// (#890) The inactivity-deadline mutex guards the hard-kill
    /// watchdog. If a panic elsewhere (e.g. the tailer, which also holds
    /// this lock) poisons it, the safety-net consumers must still read
    /// and write the deadline rather than panic — otherwise the watchdog
    /// thread dies on its next tick and the hard-kill is silently
    /// disabled. `lock_deadline` recovers the poison; the old watchdog's
    /// bare `.lock().unwrap()` would panic here.
    #[test]
    fn lock_deadline_survives_a_poisoned_mutex() {
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let deadline = Arc::new(Mutex::new(Instant::now()));

        // Poison the mutex the way a tailer panic would: panic while
        // holding the lock.
        let poisoner = Arc::clone(&deadline);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison the deadline mutex");
        })
        .join();
        assert!(
            deadline.lock().is_err(),
            "precondition: the mutex must be poisoned"
        );

        // The safety-net accessor must recover, not panic — both the
        // tailer's write and the watchdog's read.
        let want = Instant::now() + Duration::from_secs(60);
        *super::lock_deadline(&deadline) = want;
        assert_eq!(*super::lock_deadline(&deadline), want);
    }

    /// (#457) Compaction-reset: when the tailer processes a `compaction`
    /// trajectory event, it must push the shared inactivity deadline
    /// forward by `inactivity_secs`. The watchdog thread reads this
    /// deadline each tick; without the reset, productive dispatches
    /// that legitimately need many minutes between compactions get
    /// killed at the absolute initial deadline.
    ///
    /// This test exercises the `TailerState::poll_and_emit` path that
    /// fires the reset; the watchdog mechanism itself (polling + kill)
    /// is integration-tested empirically since it requires a real
    /// docker container.
    #[test]
    fn tailer_compaction_event_resets_inactivity_deadline() {
        use std::io::Write;

        let tmp = TempDir::new().unwrap();
        let traj_path = tmp.path().join("trajectory.jsonl");

        // Initialize the shared deadline to a value far in the past so
        // we can observe whether the reset moved it forward.
        let inactivity_secs = 600u64;
        let original_deadline =
            Instant::now() - Duration::from_secs(3600); // 1hr in the past
        let shared = Arc::new(Mutex::new(original_deadline));

        let mut state = TailerState::new(
            traj_path.clone(),
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
            Arc::clone(&shared),
            inactivity_secs,
        );

        // Write a compaction event to the trajectory file.
        let mut f = std::fs::File::create(&traj_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"compaction","seq":1,"generation":1,"before_messages":40,"after_messages":7,"summary_chars":1500}}"#
        )
        .unwrap();
        drop(f);

        let before_reset = Instant::now();
        state.poll_and_emit();
        let after_reset = Instant::now();

        let new_deadline = *shared.lock().unwrap();

        // The new deadline must be at least `inactivity_secs` ahead of
        // when poll_and_emit ran — confirms the reset fired. Allow a
        // small slop (1s) so the test isn't brittle on slow CI.
        let expected_min =
            before_reset + Duration::from_secs(inactivity_secs) - Duration::from_millis(50);
        let expected_max =
            after_reset + Duration::from_secs(inactivity_secs) + Duration::from_secs(1);
        assert!(
            new_deadline >= expected_min,
            "deadline must advance by ~inactivity_secs after compaction event; \
             saw new_deadline at less than expected_min"
        );
        assert!(
            new_deadline <= expected_max,
            "deadline must not advance by more than ~inactivity_secs; \
             saw new_deadline at more than expected_max (off by a multiplier?)"
        );
        // Also: the new deadline must be strictly later than the
        // original (1hr-in-the-past) — proves the reset overwrote the
        // stale value rather than no-oping.
        assert!(
            new_deadline > original_deadline,
            "reset must overwrite the prior stale deadline"
        );
    }

    /// (#457 → #464) Counter-test: events that don't indicate
    /// observable progress (model turn completions, reasoning,
    /// streaming markers) must NOT reset the inactivity deadline.
    /// Compaction and tool.completed DO reset (covered by their own
    /// tests). Per-mole-hole detectors guard against pathological
    /// tool patterns (cycle / cascade / drift / reasoning-loop).
    #[test]
    fn tailer_non_progress_events_do_not_reset_inactivity_deadline() {
        use std::io::Write;

        let tmp = TempDir::new().unwrap();
        let traj_path = tmp.path().join("trajectory.jsonl");

        let original_deadline = Instant::now() - Duration::from_secs(3600);
        let shared = Arc::new(Mutex::new(original_deadline));

        let mut state = TailerState::new(
            traj_path.clone(),
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
            Arc::clone(&shared),
            600,
        );

        // Write events that are NOT progress signals — turn boundary,
        // reasoning, streaming markers. None of these indicate the
        // model produced verified output.
        let mut f = std::fs::File::create(&traj_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"model.completed","seq":1,"finish_reason":"stop","usage":{{"completion_tokens":100}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"model.reasoning","seq":1,"reasoning_chars":500,"reasoning_text":"thinking..."}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"model.streaming.start","seq":1,"ts":1234567890}}"#
        )
        .unwrap();
        drop(f);

        state.poll_and_emit();

        let unchanged_deadline = *shared.lock().unwrap();
        assert_eq!(
            unchanged_deadline, original_deadline,
            "non-progress events (turn / reasoning / streaming) must not \
             reset the inactivity deadline; only proof-of-work signals \
             (compaction, tool.completed) qualify"
        );
    }

    /// (#464) Tool completion is the second proof-of-work signal
    /// (alongside compaction). A successful tool call — read, bash,
    /// edit, write — means the model is actively producing or
    /// inspecting state. The deadline pushes forward so productive
    /// dispatches don't get killed by a deadline that was designed
    /// around compaction frequency.
    ///
    /// Per-mole-hole detectors (cycle, cascade, drift, reasoning-loop)
    /// guard against pathological tool patterns. The deadline trusts
    /// activity; the detectors catch struggle.
    ///
    /// (#469) Resolved: the `tool.completed` schema now carries an `ok`
    /// discriminator and ONLY a successful tool call resets the deadline
    /// — see `tailer_failed_tool_completed_does_not_reset_inactivity_deadline`
    /// for the failure case. This test covers the success path (`ok:true`).
    #[test]
    fn tailer_tool_completed_event_resets_inactivity_deadline() {
        use std::io::Write;

        let tmp = TempDir::new().unwrap();
        let traj_path = tmp.path().join("trajectory.jsonl");

        let inactivity_secs = 600u64;
        let original_deadline = Instant::now() - Duration::from_secs(3600);
        let shared = Arc::new(Mutex::new(original_deadline));

        let mut state = TailerState::new(
            traj_path.clone(),
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
            Arc::clone(&shared),
            inactivity_secs,
        );

        let mut f = std::fs::File::create(&traj_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"tool.completed","seq":3,"tool_seq":0,"tool_name":"bash","args_chars":50,"result_chars":1024,"ok":true}}"#
        )
        .unwrap();
        drop(f);

        let before_reset = Instant::now();
        state.poll_and_emit();
        let after_reset = Instant::now();

        let new_deadline = *shared.lock().unwrap();
        let expected_min =
            before_reset + Duration::from_secs(inactivity_secs) - Duration::from_millis(50);
        let expected_max =
            after_reset + Duration::from_secs(inactivity_secs) + Duration::from_secs(1);
        assert!(
            new_deadline >= expected_min,
            "successful tool.completed must reset deadline by ~inactivity_secs"
        );
        assert!(
            new_deadline <= expected_max,
            "deadline must not advance more than ~inactivity_secs"
        );
        assert!(
            new_deadline > original_deadline,
            "reset must overwrite stale deadline"
        );
    }

    /// (#469) A FAILED tool call (`ok:false`) must NOT reset the
    /// inactivity deadline. This closes the fast-fail loophole: a model
    /// emitting varying failing tool calls (different args → cycle
    /// detector misses; failures interleaved with reads → failure-rate
    /// detector's consecutive count never trips) can no longer keep the
    /// deadline alive indefinitely.
    #[test]
    fn tailer_failed_tool_completed_does_not_reset_inactivity_deadline() {
        use std::io::Write;

        let tmp = TempDir::new().unwrap();
        let traj_path = tmp.path().join("trajectory.jsonl");

        let inactivity_secs = 600u64;
        let original_deadline = Instant::now() - Duration::from_secs(3600);
        let shared = Arc::new(Mutex::new(original_deadline));

        let mut state = TailerState::new(
            traj_path.clone(),
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
            Arc::clone(&shared),
            inactivity_secs,
        );

        let mut f = std::fs::File::create(&traj_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"tool.completed","seq":3,"tool_seq":0,"tool_name":"bash","args_chars":50,"result_chars":80,"ok":false}}"#
        )
        .unwrap();
        drop(f);

        state.poll_and_emit();

        let unchanged_deadline = *shared.lock().unwrap();
        assert_eq!(
            unchanged_deadline, original_deadline,
            "a failed tool.completed (ok:false) must NOT reset the deadline (#469)"
        );
    }

    /// (#469) Backward-compat: a `tool.completed` event with no `ok`
    /// field (pre-#469 trajectory) is treated as success and resets the
    /// deadline, so old data behaves as it did before the field landed.
    #[test]
    fn tailer_tool_completed_without_ok_field_resets_deadline() {
        use std::io::Write;

        let tmp = TempDir::new().unwrap();
        let traj_path = tmp.path().join("trajectory.jsonl");

        let inactivity_secs = 600u64;
        let original_deadline = Instant::now() - Duration::from_secs(3600);
        let shared = Arc::new(Mutex::new(original_deadline));

        let mut state = TailerState::new(
            traj_path.clone(),
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
            Arc::clone(&shared),
            inactivity_secs,
        );

        let mut f = std::fs::File::create(&traj_path).unwrap();
        // No `ok` field — pre-#469 shape.
        writeln!(
            f,
            r#"{{"type":"tool.completed","seq":3,"tool_seq":0,"tool_name":"bash","args_chars":50,"result_chars":1024}}"#
        )
        .unwrap();
        drop(f);

        state.poll_and_emit();

        let new_deadline = *shared.lock().unwrap();
        assert!(
            new_deadline > original_deadline,
            "missing ok defaults to success → deadline resets (backward compat)"
        );
    }

    /// (#464) Multiple proof-of-work events in one poll cycle move
    /// the deadline to the LATEST reset (not stale to the first).
    /// Compaction + tool.completed in the same poll → deadline ≈
    /// now + inactivity_secs, not stale to whichever fired first.
    #[test]
    fn tailer_multiple_proof_of_work_events_advance_to_latest() {
        use std::io::Write;

        let tmp = TempDir::new().unwrap();
        let traj_path = tmp.path().join("trajectory.jsonl");

        let inactivity_secs = 600u64;
        let original_deadline = Instant::now() - Duration::from_secs(3600);
        let shared = Arc::new(Mutex::new(original_deadline));

        let mut state = TailerState::new(
            traj_path.clone(),
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
            Arc::clone(&shared),
            inactivity_secs,
        );

        let mut f = std::fs::File::create(&traj_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"compaction","seq":1,"generation":1,"before_messages":40,"after_messages":7,"summary_chars":1500}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"tool.completed","seq":2,"tool_seq":0,"tool_name":"edit","args_chars":500,"result_chars":100}}"#
        )
        .unwrap();
        drop(f);

        let before_reset = Instant::now();
        state.poll_and_emit();

        let new_deadline = *shared.lock().unwrap();
        let expected_min =
            before_reset + Duration::from_secs(inactivity_secs) - Duration::from_millis(50);
        assert!(
            new_deadline >= expected_min,
            "deadline must reflect the latest reset, not stale to an earlier event"
        );
    }

    // ─── #557 slice 2 detector telemetry ──────────────────────────────

    /// The pure mapping helper: a `dispatch.cycle.suspected` event →
    /// `{kind:"cycle", severity:"warn", detail:<non-empty>}`. Pure (no
    /// flow sink, no IO) so the kind/severity/detail contract is asserted
    /// deterministically. All five detector kinds route through this fn.
    #[test]
    fn detector_telemetry_payload_maps_cycle_event() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "seq": 7,
            "tool_name": "read",
            "count": 3,
            "window_size": 10,
        });
        let payload =
            detector_telemetry_payload("dispatch.cycle.suspected", &event).expect("maps cycle");
        assert_eq!(payload["kind"], "cycle");
        assert_eq!(payload["severity"], "warn");
        let detail = payload["detail"].as_str().expect("detail is a string");
        assert!(!detail.is_empty(), "detail must be non-empty");
        assert!(
            detail.contains("read") && detail.contains('3') && detail.contains("10"),
            "detail must weave in the event fields; got {detail:?}"
        );
    }

    /// `intra_turn_stall.recovered` with a null `completion_tokens`
    /// (upstream omitted `usage`) renders "unknown", not a misleading 0.
    #[test]
    fn detector_telemetry_payload_renders_unknown_for_null_completion_tokens() {
        let event = serde_json::json!({
            "type": "dispatch.intra_turn_stall.recovered",
            "seq": 4,
            "completion_tokens": serde_json::Value::Null,
            "recoveries_used": 1,
            "recoveries_budget": 3,
        });
        let payload = detector_telemetry_payload("dispatch.intra_turn_stall.recovered", &event)
            .expect("maps intra-turn-stall");
        assert_eq!(payload["kind"], "intra-turn-stall");
        assert_eq!(payload["severity"], "info");
        let detail = payload["detail"].as_str().unwrap();
        assert!(
            detail.contains("unknown tokens"),
            "null completion_tokens must render as 'unknown'; got {detail:?}"
        );
    }

    /// (#795) A `model.completed` event with a full `usage` object maps
    /// to the per-turn `telemetry.tokens` payload: turn_seq carried
    /// through, total derived as prompt + completion.
    #[test]
    fn turn_tokens_payload_maps_usage_to_per_turn_payload() {
        let event = serde_json::json!({
            "type": "model.completed",
            "seq": 12,
            "finish_reason": "tool_calls",
            "usage": { "prompt_tokens": 24000, "completion_tokens": 850 },
        });
        let payload = turn_tokens_payload(&event).expect("maps usage");
        assert_eq!(payload["turn_seq"], 12);
        assert_eq!(payload["prompt_tokens"], 24000);
        assert_eq!(payload["completion_tokens"], 850);
        assert_eq!(payload["total_tokens"], 24850);
    }

    /// (#795) No `usage` (or JSON-null usage — upstream omitted it) emits
    /// NOTHING. Such turns also don't accumulate into the runtime's
    /// metrics totals, so skipping preserves the records-sum-to-total
    /// invariant rather than injecting a zero-noise record.
    #[test]
    fn turn_tokens_payload_skips_absent_or_null_usage() {
        let absent = serde_json::json!({ "type": "model.completed", "seq": 3 });
        assert!(turn_tokens_payload(&absent).is_none(), "absent usage → no record");
        let null = serde_json::json!({
            "type": "model.completed", "seq": 3, "usage": serde_json::Value::Null,
        });
        assert!(turn_tokens_payload(&null).is_none(), "null usage → no record");
    }

    /// (#795) Defensive: a `usage` object missing a count degrades that
    /// count to 0 (the runtime always writes both fields; this guards
    /// hand-rolled or cross-runtime trajectories).
    #[test]
    fn turn_tokens_payload_defaults_missing_counts_to_zero() {
        let event = serde_json::json!({
            "type": "model.completed",
            "seq": 1,
            "usage": { "completion_tokens": 500 },
        });
        let payload = turn_tokens_payload(&event).expect("partial usage still maps");
        assert_eq!(payload["prompt_tokens"], 0);
        assert_eq!(payload["completion_tokens"], 500);
        assert_eq!(payload["total_tokens"], 500);
    }

    /// Integration shape: feed a `dispatch.cycle.suspected` trajectory
    /// line through `handle_event` and assert the emitted FlowRecord is a
    /// telemetry record (`category:"telemetry"`, `source:"detector"`)
    /// with a `kind:"cycle"`/`severity:"warn"`/non-empty `detail` payload.
    ///
    /// Capture mechanism: `LocalFileSink` resolves `DARKMUX_FLOWS_DIR`
    /// per write (see the #507 note on the sink), so pointing it at a
    /// tempdir and reading the day-file back observes exactly the record
    /// `handle_event` → `emit_telemetry` → `darkmux_flow::record` wrote.
    /// `#[serial]` guards the shared env var (other flow tests mutate it).
    /// (#717) Read every flow record the default sink wrote to a tempdir
    /// day-file. Shared helper for the bookend-guard tests below.
    fn drain_flow_records(dir: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                    && p.file_name().and_then(|n| n.to_str()) != Some("trajectory.jsonl")
            })
            .flat_map(|p| std::fs::read_to_string(&p).unwrap_or_default().into_bytes())
            .collect::<Vec<u8>>()
            .split(|&b| b == b'\n')
            .filter_map(|l| serde_json::from_slice::<serde_json::Value>(l).ok())
            .collect()
    }

    #[test]
    #[serial]
    fn bookend_guard_armed_emits_dispatch_error_on_drop() {
        // An armed guard dropped without disarming (the `?`-return / panic
        // path) emits a `dispatch.error` terminal carrying the same mission
        // so the orphaned start is bookended + stays grouped (#717/#714).
        let tmp = TempDir::new().unwrap();
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        {
            let _guard = DispatchBookendGuard {
                armed: true,
                role_id: "coder".into(),
                session_id: "sess-orphan".into(),
                model: "darkmux:qwen3.6".into(),
                mission_id: Some("pre-1.0-compat-sweep".into()),
                sprint_id: Some("s694".into()),
            };
            // drop here (end of scope) — no disarm
        }

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        let rec = drain_flow_records(tmp.path())
            .into_iter()
            .find(|v| v["action"] == "dispatch error")
            .expect("armed guard should emit a dispatch.error terminal on drop");
        assert_eq!(rec["session_id"], "sess-orphan");
        assert_eq!(rec["mission_id"], "pre-1.0-compat-sweep");
        assert_eq!(rec["sprint_id"], "s694");
        assert_eq!(rec["payload"]["result_class"], "error");
    }

    #[test]
    #[serial]
    fn bookend_guard_disarmed_emits_nothing_on_drop() {
        // The happy path (and container-ran-but-failed path) disarm after
        // their own terminal record — the guard must then stay silent so the
        // dispatch isn't double-counted.
        let tmp = TempDir::new().unwrap();
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        {
            let mut guard = DispatchBookendGuard {
                armed: true,
                role_id: "coder".into(),
                session_id: "sess-clean".into(),
                model: "darkmux:qwen3.6".into(),
                mission_id: None,
                sprint_id: None,
            };
            guard.disarm();
        }

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        let emitted = drain_flow_records(tmp.path())
            .into_iter()
            .any(|v| v["action"] == "dispatch error");
        assert!(!emitted, "disarmed guard must not emit any terminal record");
    }

    #[test]
    #[serial]
    fn bookend_guard_fires_on_panic_unwind() {
        // The RAII headline: a panic between start and disarm still bookends
        // the start. Rust runs Drop on unwind, so the guard emits its
        // dispatch.error even when the dispatch panics mid-flight (#717).
        let tmp = TempDir::new().unwrap();
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        // Silence the expected panic backtrace so test output stays clean.
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(|| {
            let _guard = DispatchBookendGuard {
                armed: true,
                role_id: "coder".into(),
                session_id: "sess-panic".into(),
                model: "darkmux:qwen3.6".into(),
                mission_id: Some("pre-1.0-compat-sweep".into()),
                sprint_id: None,
            };
            panic!("simulated mid-dispatch panic");
        });
        std::panic::set_hook(prev_hook);
        assert!(result.is_err(), "the closure should have panicked");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        let rec = drain_flow_records(tmp.path())
            .into_iter()
            .find(|v| v["action"] == "dispatch error")
            .expect("guard should emit a dispatch.error terminal on panic unwind");
        assert_eq!(rec["session_id"], "sess-panic");
        assert_eq!(rec["mission_id"], "pre-1.0-compat-sweep");
    }

    #[test]
    #[serial]
    fn handle_event_cycle_emits_telemetry_record() {
        let tmp = TempDir::new().unwrap();
        // SAFETY: serialized via `#[serial]`; no concurrent env reader.
        // Scrub DARKMUX_REDIS_URL so a stray operator-shell value can't
        // make the default-sink write block on an unreachable peer
        // (the 75s/record timeout the flow crate's isolate helper guards
        // against; we inline the scrub since that helper is test-support
        // gated and not visible here).
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        let mut state = TailerState::new_for_test(
            traj_path,
            "sess-cycle".into(),
            "coder".into(),
            "darkmux:qwen3.6".into(),
        );
        state.handle_event(
            r#"{"type":"dispatch.cycle.suspected","seq":9,"tool_name":"read","canonical_args":"{}","count":3,"window_size":10}"#,
        );

        // Restore env BEFORE assertions so a failing assert can't leak it.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        // Find the day-file the sink wrote (YYYY-MM-DD.jsonl) without
        // needing the crate-private day helper — glob the tempdir.
        let day_file = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| {
                p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                    && p.file_name().and_then(|n| n.to_str()) != Some("trajectory.jsonl")
            })
            .expect("a flow day-file should have been written");

        let contents = std::fs::read_to_string(&day_file).unwrap();
        let telemetry = contents
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["category"] == "telemetry")
            .expect("a telemetry record should have been emitted");

        assert_eq!(telemetry["category"], "telemetry");
        assert_eq!(telemetry["source"], "detector");
        assert_eq!(telemetry["action"], "telemetry.detector");
        assert_eq!(telemetry["handle"], "coder");
        assert_eq!(telemetry["payload"]["kind"], "cycle");
        assert_eq!(telemetry["payload"]["severity"], "warn");
        assert!(
            telemetry["payload"]["detail"]
                .as_str()
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "detail must be present and non-empty"
        );
    }

    /// (#795) A `model.completed` trajectory line with usage → BOTH a
    /// `dispatch.turn` Work record AND a per-turn `category=telemetry,
    /// source=tokens` record carrying that turn's billed usage + turn_seq.
    /// Same capture mechanism + env-scrub as the cycle test above.
    #[test]
    #[serial]
    fn handle_event_model_completed_emits_per_turn_tokens_telemetry() {
        let tmp = TempDir::new().unwrap();
        // SAFETY: serialized via `#[serial]`; no concurrent env reader.
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        let mut state = TailerState::new_for_test(
            traj_path,
            "sess-tokens".into(),
            "coder".into(),
            "darkmux:qwen3.6".into(),
        );
        state.handle_event(
            r#"{"type":"model.completed","seq":5,"finish_reason":"tool_calls","usage":{"prompt_tokens":31000,"completion_tokens":1200}}"#,
        );
        // A no-usage turn must emit dispatch.turn but NO tokens record.
        state.handle_event(r#"{"type":"model.completed","seq":6,"finish_reason":"stop"}"#);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        let records = drain_flow_records(tmp.path());
        let tokens: Vec<&serde_json::Value> = records
            .iter()
            .filter(|v| v["category"] == "telemetry" && v["source"] == "tokens")
            .collect();
        assert_eq!(
            tokens.len(),
            1,
            "exactly one tokens record (the no-usage turn must not emit one); got {tokens:?}"
        );
        let rec = tokens[0];
        assert_eq!(rec["action"], "telemetry.tokens");
        assert_eq!(rec["handle"], "coder");
        assert_eq!(rec["session_id"], "sess-tokens");
        assert_eq!(rec["payload"]["turn_seq"], 5);
        assert_eq!(rec["payload"]["prompt_tokens"], 31000);
        assert_eq!(rec["payload"]["completion_tokens"], 1200);
        assert_eq!(rec["payload"]["total_tokens"], 32200);
        // Both turns still produced their dispatch.turn Work records.
        let turns = records.iter().filter(|v| v["action"] == "dispatch.turn").count();
        assert_eq!(turns, 2, "dispatch.turn unaffected by the telemetry emission");
    }

    /// (#557 slice 3) A `dispatch.context` trajectory line → a
    /// `category=telemetry, source=context` flow record carrying
    /// `{used, max}`. Same capture mechanism + env-scrub as the cycle
    /// test above (tempdir DARKMUX_FLOWS_DIR + DARKMUX_REDIS_URL scrub +
    /// `#[serial]`).
    #[test]
    #[serial]
    fn handle_event_context_emits_telemetry_record() {
        let tmp = TempDir::new().unwrap();
        // SAFETY: serialized via `#[serial]`; no concurrent env reader.
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        let mut state = TailerState::new_for_test(
            traj_path,
            "sess-context".into(),
            "coder".into(),
            "darkmux:qwen3.6".into(),
        );
        state.handle_event(
            r#"{"type":"dispatch.context","seq":3,"used":42000,"max":101000}"#,
        );

        // Restore env BEFORE assertions so a failing assert can't leak it.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        let day_file = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| {
                p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                    && p.file_name().and_then(|n| n.to_str()) != Some("trajectory.jsonl")
            })
            .expect("a flow day-file should have been written");

        let contents = std::fs::read_to_string(&day_file).unwrap();
        // Scope to THIS test's session_id — the process-global
        // `DARKMUX_FLOWS_DIR` is shared with concurrent non-serial tailing
        // tests that also write telemetry records here.
        let telemetry = contents
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| {
                v["category"] == "telemetry"
                    && v["source"] == "context"
                    && v["session_id"] == "sess-context"
            })
            .expect("a telemetry record should have been emitted");

        assert_eq!(telemetry["category"], "telemetry");
        assert_eq!(telemetry["source"], "context");
        assert_eq!(telemetry["action"], "telemetry.context");
        assert_eq!(telemetry["payload"]["used"], 42000);
        assert_eq!(telemetry["payload"]["max"], 101000);
    }

    /// (#557 slice 3) A `compaction` trajectory line carrying
    /// `tokens_before`/`tokens_after` → BOTH the existing
    /// `dispatch.compaction` Work record (category=work) AND a new
    /// `source=compaction` telemetry record reading `{from, to}`.
    #[test]
    #[serial]
    fn handle_event_compaction_emits_work_and_telemetry_records() {
        let tmp = TempDir::new().unwrap();
        // SAFETY: serialized via `#[serial]`; no concurrent env reader.
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        let mut state = TailerState::new_for_test(
            traj_path,
            "sess-compaction".into(),
            "coder".into(),
            "darkmux:qwen3.6".into(),
        );
        state.handle_event(
            r#"{"type":"compaction","generation":1,"before_messages":30,"after_messages":6,"summary_chars":1500,"tokens_before":48000,"tokens_after":9000}"#,
        );

        // Restore env BEFORE assertions so a failing assert can't leak it.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        let day_file = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| {
                p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                    && p.file_name().and_then(|n| n.to_str()) != Some("trajectory.jsonl")
            })
            .expect("a flow day-file should have been written");

        let contents = std::fs::read_to_string(&day_file).unwrap();
        let records: Vec<serde_json::Value> = contents
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .collect();

        // The existing dispatch.compaction Work record must still fire.
        let work = records
            .iter()
            .find(|v| {
                v["action"] == "dispatch.compaction"
                    && v["session_id"] == "sess-compaction"
            })
            .expect("the dispatch.compaction work record should still be emitted");
        assert_eq!(work["payload"]["generation"], 1);
        assert_eq!(work["payload"]["before_messages"], 30);
        assert_eq!(work["payload"]["after_messages"], 6);

        // The new compaction telemetry record carries {from, to}.
        // Scope to THIS test's session_id: the process-global
        // `DARKMUX_FLOWS_DIR` (set above) is also the write target for any
        // non-serial live-tailing test that runs concurrently and emits
        // `test-role`/`test-session` records — including compaction lines
        // without tokens_before/after. Match on our unique session so the
        // assertion can't latch onto a foreign record.
        let telemetry = records
            .iter()
            .find(|v| {
                v["category"] == "telemetry"
                    && v["source"] == "compaction"
                    && v["session_id"] == "sess-compaction"
            })
            .expect("a source=compaction telemetry record should have been emitted");
        assert_eq!(telemetry["action"], "telemetry.compaction");
        assert_eq!(telemetry["payload"]["from"], 48000);
        assert_eq!(telemetry["payload"]["to"], 9000);
    }

    // ─── role tool_palette → runtime allowed-tools mapping ────────────

    fn palette(allow: &[&str], deny: &[&str]) -> crate::types::ToolPalette {
        crate::types::ToolPalette {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn allowed_tools_empty_palette_returns_none_so_runtime_uses_full_catalog() {
        let p = palette(&[], &[]);
        assert_eq!(compute_runtime_allowed_tools(&p), None);
    }

    #[test]
    fn allowed_tools_coder_palette_exposes_all_runtime_tools() {
        // coder role: allow [read, edit, write, exec, process], deny []
        let p = palette(&["read", "edit", "write", "exec", "process"], &[]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        // Expected: read + search (from "read"), edit, write, bash (from "exec").
        // "process" has no runtime equivalent; silently dropped.
        let mut sorted = result.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["bash", "edit", "read", "search", "write"]);
    }

    #[test]
    fn allowed_tools_code_reviewer_palette_excludes_edit_and_write() {
        // code-reviewer: allow [read, exec, update_plan], deny [edit, write, process]
        let p = palette(&["read", "exec", "update_plan"], &["edit", "write", "process"]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        let mut sorted = result.clone();
        sorted.sort();
        // Expected: read + search (from "read"), bash (from "exec").
        // "update_plan" has no runtime equivalent.
        assert_eq!(sorted, vec!["bash", "read", "search"]);
        // Hard regression guard: code-reviewer must NEVER see edit/write.
        assert!(!result.contains(&"edit".to_string()));
        assert!(!result.contains(&"write".to_string()));
    }

    #[test]
    fn allowed_tools_deny_overrides_allow() {
        // Pathological: same tool in both lists. Deny wins.
        let p = palette(&["edit"], &["edit"]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert!(result.is_empty(), "deny must win over allow; got {result:?}");
    }

    #[test]
    fn allowed_tools_unknown_role_vocab_silently_dropped() {
        let p = palette(&["fake-tool", "not-a-thing"], &[]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert!(result.is_empty(), "unknown role-vocab → empty; got {result:?}");
    }

    #[test]
    fn allowed_tools_role_read_expands_to_runtime_read_and_search() {
        // Conceptual contract: role "read" means "the model may read";
        // runtime "search" is a specialized read (find pattern in tree)
        // that's implied by the broader "read" allowance.
        let p = palette(&["read"], &[]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        let mut sorted = result.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["read", "search"]);
    }

    // ─── #340: unknown role-vocab token detection ───────────────────

    #[test]
    fn unknown_role_vocab_empty_palette_returns_empty() {
        let p = palette(&[], &[]);
        assert!(unknown_role_vocab_tokens(&p).is_empty());
    }

    #[test]
    fn unknown_role_vocab_all_known_tokens_returns_empty() {
        let p = palette(&["read", "edit", "write"], &["exec", "process", "update_plan"]);
        assert!(unknown_role_vocab_tokens(&p).is_empty());
    }

    /// Typo in allow list (the canonical failure shape from #340 spec —
    /// "exce" instead of "exec" silently drops the agent's only exec
    /// capability).
    #[test]
    fn unknown_role_vocab_typo_in_allow_is_surfaced() {
        let p = palette(&["read", "exce"], &[]);
        assert_eq!(unknown_role_vocab_tokens(&p), vec!["exce".to_string()]);
    }

    /// Typos in both lists are deduplicated + sorted.
    #[test]
    fn unknown_role_vocab_dedupes_and_sorts_across_allow_and_deny() {
        let p = palette(&["fake-tool", "exce"], &["fake-tool", "another-typo"]);
        let unknowns = unknown_role_vocab_tokens(&p);
        assert_eq!(
            unknowns,
            vec![
                "another-typo".to_string(),
                "exce".to_string(),
                "fake-tool".to_string(),
            ]
        );
    }

    /// Future tokens (vendor-specific, not yet wired) are flagged as
    /// unknown — operator-facing signal that the manifest references
    /// a token darkmux doesn't know how to map yet. NOT a hard error;
    /// the dispatch can still proceed (the unknown just drops from
    /// the runtime catalog) but the operator gets warned.
    #[test]
    fn unknown_role_vocab_future_token_is_flagged() {
        let p = palette(&["read", "vendor-specific-tool"], &[]);
        assert_eq!(
            unknown_role_vocab_tokens(&p),
            vec!["vendor-specific-tool".to_string()]
        );
    }

    /// Known tokens (including those with no runtime equivalent like
    /// `process` and `update_plan`) MUST NOT be flagged. They're
    /// intentionally dropped, not accidentally.
    #[test]
    fn unknown_role_vocab_does_not_flag_known_no_runtime_tokens() {
        let p = palette(&["process", "update_plan"], &[]);
        assert!(
            unknown_role_vocab_tokens(&p).is_empty(),
            "process and update_plan are known role-vocab; should NOT be flagged"
        );
    }

    #[test]
    fn known_role_vocab_csv_contains_all_known_tokens() {
        let csv = known_role_vocab_csv();
        for token in ["read", "edit", "write", "exec", "process", "update_plan"] {
            assert!(
                csv.contains(token),
                "known_role_vocab_csv missing `{token}`; got: {csv}"
            );
        }
    }

    #[test]
    fn allowed_tools_role_exec_maps_to_runtime_bash() {
        let p = palette(&["exec"], &[]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert_eq!(result, vec!["bash".to_string()]);
    }

    /// QA NIT 1 — deny strips ALL of a role-vocab token's runtime
    /// expansions, not just the literal name. Pins the contract: if a
    /// future refactor switched deny to "literal-string only," role
    /// "read" denied would still leak `search` (which expands from
    /// "read"). Regression guard for the expansion-stripping invariant.
    #[test]
    fn allowed_tools_deny_role_read_strips_both_read_and_search() {
        let p = palette(&["read"], &["read"]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert!(
            result.is_empty(),
            "denying role-vocab `read` must strip BOTH runtime `read` and `search`; got {result:?}"
        );
    }

    /// Sibling: partial overlap. `allow:["read","exec"], deny:["read"]`
    /// must result in `["bash"]` only — both `read` and `search` removed
    /// by the deny.
    #[test]
    fn allowed_tools_deny_role_read_alongside_allowed_exec_leaves_only_bash() {
        let p = palette(&["read", "exec"], &["read"]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert_eq!(result, vec!["bash".to_string()]);
    }

    // ─── cap_reasoning_text (S6) ──────────────────────────────────────

    #[test]
    fn cap_reasoning_text_passes_through_short_string() {
        let v = serde_json::Value::String("short".into());
        let out = cap_reasoning_text(Some(&v));
        assert_eq!(out, v);
    }

    #[test]
    fn cap_reasoning_text_passes_through_null() {
        assert_eq!(cap_reasoning_text(None), serde_json::Value::Null);
    }

    #[test]
    fn cap_reasoning_text_passes_through_non_string() {
        let v = serde_json::Value::Number(42.into());
        let out = cap_reasoning_text(Some(&v));
        assert_eq!(out, v);
    }

    #[test]
    fn cap_reasoning_text_truncates_oversize_and_marks() {
        let oversize = "x".repeat(MAX_REASONING_TEXT_BYTES + 100);
        let v = serde_json::Value::String(oversize.clone());
        let out = cap_reasoning_text(Some(&v));
        let s = out.as_str().expect("output is string");
        assert!(s.len() < oversize.len(), "must be shorter than input");
        assert!(s.contains("[truncated"), "must carry truncation marker");
        assert!(s.contains(&oversize.len().to_string()), "marker must include original byte count");
    }

    #[test]
    fn cap_reasoning_text_truncates_at_utf8_boundary() {
        // Build a string where the byte just past the cap is mid-codepoint
        // (4-byte emoji starting at a position near the cap). Result must
        // still be valid UTF-8.
        let pad_bytes = MAX_REASONING_TEXT_BYTES - 1;
        let mut s = "a".repeat(pad_bytes);
        s.push('🦀'); // 4 bytes, starts at pad_bytes
        s.push_str(&"b".repeat(50));
        let v = serde_json::Value::String(s);
        let out = cap_reasoning_text(Some(&v));
        let truncated = out.as_str().expect("output is string");
        // The marker is appended; the actual truncated content is valid UTF-8
        // because String::from_utf8_lossy isn't used — we sliced on a boundary.
        assert!(truncated.is_char_boundary(0));
        assert!(truncated.contains("[truncated"));
    }

    // ─── #237: bounding container-written trajectory fields at ingest ──
    #[test]
    fn cap_json_str_bounds_short_fields() {
        // A container could write a pathologically large tool_name / finish_reason.
        let huge = "z".repeat(MAX_TRAJ_FIELD_BYTES + 5000);
        let v = serde_json::Value::String(huge.clone());
        let out = cap_json_str(Some(&v), MAX_TRAJ_FIELD_BYTES);
        let s = out.as_str().expect("string out");
        assert!(s.len() <= MAX_TRAJ_FIELD_BYTES + 100, "bounded near the cap (+marker)");
        assert!(s.contains("[truncated"), "carries the marker");
        assert!(s.contains(&huge.len().to_string()), "marker names the original size");
        // Short values are untouched.
        let small = serde_json::json!("read");
        assert_eq!(cap_json_str(Some(&small), MAX_TRAJ_FIELD_BYTES), small);
        // Non-string + None pass through / null.
        let n = serde_json::json!(42);
        assert_eq!(cap_json_str(Some(&n), MAX_TRAJ_FIELD_BYTES), n);
        assert_eq!(cap_json_str(None, MAX_TRAJ_FIELD_BYTES), serde_json::Value::Null);
    }

    #[test]
    fn detector_detail_is_bounded_against_oversize_tool_name() {
        // A container-injected cycle event with a giant tool_name must not
        // produce an unbounded detector `detail` in the telemetry record.
        let huge = "t".repeat(100_000);
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": huge,
            "count": 3,
            "window_size": 10,
        });
        let payload = detector_telemetry_payload("dispatch.cycle.suspected", &event)
            .expect("cycle event yields a payload");
        let detail = payload["detail"].as_str().expect("detail string");
        assert!(detail.len() <= MAX_TRAJ_FIELD_BYTES + 100, "detail bounded near the cap");
        assert!(detail.contains("[truncated"), "carries the marker");
        assert_eq!(payload["kind"], "cycle");
        assert_eq!(payload["severity"], "warn");
    }

    // ─── #994 engagement-context capture (slice 1): area.files ────────

    /// A cycle on a file-bearing tool (edit/write/read/search) stamps
    /// `area.files` with the path the runtime canonicalized into the event,
    /// keying the caution to the file it happened in.
    #[test]
    fn detector_telemetry_payload_stamps_area_files_for_file_cycle() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "seq": 9,
            "tool_name": "edit",
            "canonical_args": r#"{"path":"src/lib.rs"}"#,
            "count": 3,
            "window_size": 10,
        });
        let payload =
            detector_telemetry_payload("dispatch.cycle.suspected", &event).expect("maps cycle");
        assert_eq!(
            payload["area"]["files"],
            serde_json::json!(["src/lib.rs"]),
            "a file-bearing cycle keys the caution to the edited path"
        );
    }

    /// A `bash` cycle carries `{command: …}` — no file — so no `area` is
    /// stamped (a fileless area would be noise; the firing stays
    /// engagement-level, not pinned to a file that doesn't exist).
    #[test]
    fn detector_telemetry_payload_omits_area_for_fileless_cycle() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": "bash",
            "canonical_args": r#"{"command":"ls -la"}"#,
            "count": 3,
            "window_size": 10,
        });
        let payload =
            detector_telemetry_payload("dispatch.cycle.suspected", &event).expect("maps cycle");
        assert!(payload.get("area").is_none(), "bash cycle has no file area");
    }

    /// A `search` cycle DOES carry a `path` in its canonical args — but it's
    /// the search *root directory*, not a target file. The tool allowlist must
    /// exclude it so a directory is never stamped into `area.files` (the
    /// category error CONSIDER-1 in the #994-capture QA caught).
    #[test]
    fn detector_telemetry_payload_omits_area_for_search_cycle_directory_path() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": "search",
            "canonical_args": r#"{"pattern":"TODO","path":"src/"}"#,
            "count": 3,
            "window_size": 10,
        });
        let payload =
            detector_telemetry_payload("dispatch.cycle.suspected", &event).expect("maps cycle");
        assert!(
            payload.get("area").is_none(),
            "search's path is a directory, not a file — must not be stamped as area.files"
        );
    }

    /// Malformed `canonical_args` degrade to no `area` rather than dropping the
    /// firing or panicking — the detail/kind/severity still render.
    #[test]
    fn detector_telemetry_payload_omits_area_on_malformed_canonical_args() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": "edit",
            "canonical_args": "not json{{",
            "count": 3,
            "window_size": 10,
        });
        let payload = detector_telemetry_payload("dispatch.cycle.suspected", &event)
            .expect("maps cycle even with bad args");
        assert!(payload.get("area").is_none());
        assert_eq!(payload["kind"], "cycle");
    }

    /// A cycle event with no `canonical_args` at all (the pre-#994 event shape
    /// the other detector tests use) stamps no `area` — guards those existing
    /// assertions against this slice.
    #[test]
    fn detector_telemetry_payload_omits_area_when_canonical_args_absent() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": "read",
            "count": 3,
            "window_size": 10,
        });
        let payload =
            detector_telemetry_payload("dispatch.cycle.suspected", &event).expect("maps cycle");
        assert!(payload.get("area").is_none());
    }

    /// Turn-level detectors fire with no single target file, so they never
    /// stamp `area` (engagement-level cautions in #994 terms).
    #[test]
    fn detector_telemetry_payload_omits_area_for_turn_level_detector() {
        let event = serde_json::json!({
            "type": "dispatch.reasoning_loop.suspected",
            "count": 4,
            "window_size": 6,
        });
        let payload = detector_telemetry_payload("dispatch.reasoning_loop.suspected", &event)
            .expect("maps reasoning-loop");
        assert!(payload.get("area").is_none());
    }

    /// A pathologically long container-written path is bounded the same way the
    /// detector `detail` is (#237) — it can't bloat the telemetry record.
    #[test]
    fn detector_area_path_is_bounded() {
        let huge = "p".repeat(100_000);
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": "edit",
            "canonical_args": serde_json::json!({ "path": huge }).to_string(),
            "count": 3,
            "window_size": 10,
        });
        let payload =
            detector_telemetry_payload("dispatch.cycle.suspected", &event).expect("maps cycle");
        let file = payload["area"]["files"][0].as_str().expect("file string");
        assert!(file.len() <= MAX_TRAJ_FIELD_BYTES + 100, "path bounded near the cap");
        assert!(file.contains("[truncated"), "carries the marker");
    }

    /// (#1001) The firing-time `code_hash` the runtime captures is forwarded
    /// into `area.code_hash` (for staleness ranking), alongside the file.
    #[test]
    fn detector_area_forwards_code_hash() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": "edit",
            "canonical_args": r#"{"path":"src/a.rs"}"#,
            "code_hash": "deadbeef",
            "count": 3,
            "window_size": 10,
        });
        let area = detector_area("dispatch.cycle.suspected", &event).expect("has area");
        assert_eq!(area["files"][0], "src/a.rs");
        assert_eq!(area["code_hash"], "deadbeef");
    }

    /// (#1001) A cycle on a file tool with no captured hash (non-code target)
    /// still yields the file area, just without `code_hash`.
    #[test]
    fn detector_area_omits_code_hash_when_absent() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": "read",
            "canonical_args": r#"{"path":"src/a.rs"}"#,
            "count": 3,
            "window_size": 10,
        });
        let area = detector_area("dispatch.cycle.suspected", &event).expect("has area");
        assert_eq!(area["files"][0], "src/a.rs");
        assert!(area.get("code_hash").is_none(), "no code_hash key when uncaptured");
    }

    /// (#1001) The tool-failure-cascade detector now carries `canonical_args`,
    /// so a failure on a file-editing tool keys to its file + code_hash like a
    /// cycle does; a failure on a non-file tool (`bash`) is engagement-level.
    #[test]
    fn detector_area_maps_tool_repeated_failure() {
        let edit_fail = serde_json::json!({
            "type": "dispatch.tool.repeated_failure",
            "tool_name": "edit",
            "canonical_args": r#"{"path":"src/b.rs"}"#,
            "code_hash": "cafe",
            "failure_count": 3,
        });
        let area = detector_area("dispatch.tool.repeated_failure", &edit_fail).expect("file area");
        assert_eq!(area["files"][0], "src/b.rs");
        assert_eq!(area["code_hash"], "cafe");

        let bash_fail = serde_json::json!({
            "type": "dispatch.tool.repeated_failure",
            "tool_name": "bash",
            "canonical_args": r#"{"command":"ls"}"#,
            "failure_count": 3,
        });
        assert!(
            detector_area("dispatch.tool.repeated_failure", &bash_fail).is_none(),
            "a bash failure is engagement-level, not file-scoped"
        );
    }

    /// `extract_tool_target_path` mirrors the runtime parser: pulls a string
    /// `path`; `None` on missing / non-string / malformed.
    #[test]
    fn extract_tool_target_path_pulls_path_and_degrades() {
        assert_eq!(
            extract_tool_target_path(r#"{"path":"a/b.rs","offset":1}"#).as_deref(),
            Some("a/b.rs")
        );
        assert!(extract_tool_target_path(r#"{"command":"ls"}"#).is_none());
        assert!(extract_tool_target_path(r#"{"path":42}"#).is_none());
        assert!(extract_tool_target_path("not json").is_none());
    }

    // ─── TailerState::poll_and_emit (live tailing) ────────────────────

    fn fixture_state(trajectory_path: PathBuf) -> TailerState {
        TailerState::new_for_test(
            trajectory_path,
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
        )
    }

    #[test]
    fn tailer_state_with_mission_stamps_fields() {
        // (#714) The production tailer chains `.with_mission(...)` so every
        // per-event flow record it emits carries the dispatch's mission/sprint
        // and groups under the mission in the observability view. Default
        // (test/one-off) is None.
        let tmp = TempDir::new().unwrap();
        let bare = fixture_state(tmp.path().join("t.jsonl"));
        assert!(bare.mission_id.is_none() && bare.sprint_id.is_none());

        let stamped = fixture_state(tmp.path().join("t.jsonl")).with_mission(
            Some("pre-1.0-compat-sweep".into()),
            Some("s694-profiles-schema".into()),
        );
        assert_eq!(stamped.mission_id.as_deref(), Some("pre-1.0-compat-sweep"));
        assert_eq!(stamped.sprint_id.as_deref(), Some("s694-profiles-schema"));
    }

    #[test]
    fn tailer_state_handles_missing_file() {
        // poll_and_emit must be a no-op when the trajectory file doesn't
        // exist yet (container hasn't written anything).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("never-written.jsonl");
        let mut state = fixture_state(path);
        state.poll_and_emit(); // no panic; no events
        assert_eq!(state.offset, 0);
        assert!(state.pending.is_empty());
    }

    #[test]
    fn tailer_state_carries_partial_line_across_polls() {
        // Write the first half of a line, poll, write the second half,
        // poll again — the state's pending buffer must stitch them together
        // and only dispatch the event once the newline arrives.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        // First write: incomplete (no newline)
        {
            let mut f = std::fs::File::create(&path).unwrap();
            write!(f, "{{\"type\":\"model.compl").unwrap();
        }
        state.poll_and_emit();
        assert_eq!(state.summary.turns, 0, "no complete line yet");
        assert!(!state.pending.is_empty(), "partial line carried");

        // Second write: appends the rest of the line with newline
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, "eted\",\"seq\":1,\"finish_reason\":\"stop\"}}").unwrap();
        }
        state.poll_and_emit();
        assert_eq!(state.summary.turns, 1, "complete line dispatched after second poll");
        assert!(state.pending.is_empty(), "pending drained after newline");
    }

    /// Regression guard for #329 — multi-byte UTF-8 characters split
    /// across reads must not corrupt to U+FFFD.
    ///
    /// The `drain_complete_lines_from_bytes` helper is the pure-
    /// function extract that makes the bug directly testable. Before
    /// the fix: pending was String; each poll did from_utf8_lossy on
    /// partial bytes; emoji split across the boundary became U+FFFD.
    /// After the fix: pending is Vec<u8>; decode happens once per
    /// complete line.
    #[test]
    fn drain_complete_lines_preserves_multibyte_across_extends() {
        let mut pending: Vec<u8> = Vec::new();

        // First chunk: prefix + first 2 bytes of 🦀.
        pending.extend_from_slice(b"{\"reasoning_text\":\"");
        pending.extend_from_slice(b"\xF0\x9F");
        let lines = drain_complete_lines_from_bytes(&mut pending);
        assert!(lines.is_empty(), "no newline yet — nothing drained");

        // Second chunk: last 2 bytes of 🦀, close out, newline.
        pending.extend_from_slice(b"\xA6\x80 reactor\"}\n");
        let lines = drain_complete_lines_from_bytes(&mut pending);
        assert_eq!(lines.len(), 1, "complete line drained");
        assert!(
            lines[0].contains("🦀 reactor"),
            "multi-byte char must round-trip intact; got: {}",
            lines[0]
        );
        assert!(
            !lines[0].contains('\u{FFFD}'),
            "no replacement chars should appear; got: {}",
            lines[0]
        );
        assert!(pending.is_empty(), "pending drained after newline");
    }

    /// Two complete lines in one buffer, plus a partial third line.
    /// The helper must drain both complete lines and leave the
    /// partial third in pending.
    #[test]
    fn drain_complete_lines_handles_multiple_lines_per_call() {
        let mut pending: Vec<u8> = b"line one\nline two\npartial".to_vec();
        let lines = drain_complete_lines_from_bytes(&mut pending);
        assert_eq!(lines, vec!["line one".to_string(), "line two".to_string()]);
        assert_eq!(pending, b"partial");
    }

    /// Empty lines (consecutive newlines) are skipped — matches the
    /// pre-fix behavior of the line-emit loop.
    #[test]
    fn drain_complete_lines_skips_empty_lines() {
        let mut pending: Vec<u8> = b"alpha\n\nbeta\n".to_vec();
        let lines = drain_complete_lines_from_bytes(&mut pending);
        assert_eq!(lines, vec!["alpha".to_string(), "beta".to_string()]);
        assert!(pending.is_empty());
    }

    /// End-to-end through TailerState: write a line with an emoji
    /// split across two polls; the tailer's handle_event sees the
    /// intact line. Verified by writing a model.completed event
    /// (which the summary DOES track) interleaved with the emoji
    /// line — the turn count proves the second line was parsed.
    #[test]
    fn tailer_state_dispatches_event_after_multibyte_split() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        // First write: model.completed line intact + start of a
        // second line containing 🦀 (4-byte UTF-8 seq), broken
        // mid-codepoint.
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(b"{\"type\":\"model.completed\",\"seq\":1,\"finish_reason\":\"stop\"}\n")
                .unwrap();
            f.write_all(b"{\"type\":\"model.reasoning\",\"reasoning_text\":\"")
                .unwrap();
            f.write_all(b"\xF0\x9F").unwrap();
        }
        state.poll_and_emit();
        assert_eq!(state.summary.turns, 1, "first line dispatched");

        // Second write: completes the 🦀 + closes the JSON.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"\xA6\x80 reactor\"}\n").unwrap();
        }
        state.poll_and_emit();
        // pending should be empty — both lines now drained.
        assert!(
            state.pending.is_empty(),
            "all lines drained after second poll; got pending={:?}",
            state.pending
        );
    }

    #[test]
    fn tailer_state_resets_on_truncation() {
        // Defensive path: if the file shrinks below our offset, the
        // tailer must reset its offset to 0 rather than trying to seek
        // past EOF.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        std::fs::write(&path, b"some-bytes\n").unwrap();
        state.poll_and_emit();
        let offset_before = state.offset;
        assert!(offset_before > 0);

        // Truncate to a smaller size.
        std::fs::write(&path, b"").unwrap();
        state.poll_and_emit();
        // After truncation poll, offset should reset to 0 (file is empty,
        // so 0 ≤ size = 0 and offset is 0).
        assert_eq!(state.offset, 0);
    }

    #[test]
    fn tailer_skips_malformed_lines() {
        // A non-JSON line in the trajectory must not crash the tailer or
        // stop later events from being processed.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        let lines = "not json\n\
            {\"type\":\"tool.completed\",\"tool_seq\":1,\"tool_name\":\"bash\"}\n";
        std::fs::write(&path, lines).unwrap();
        state.poll_and_emit();
        assert_eq!(state.summary.tool_calls, 1, "later valid event still processed");
    }

    // ─── Heartbeat rate limiting ──────────────────────────────────────

    #[test]
    fn heartbeat_first_partial_emits() {
        // The very first model.partial should produce a heartbeat (no
        // prior last_heartbeat_at).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        let line = r#"{"type":"model.partial","seq":1,"partial_index":0,"cumulative_chars":10}"#;
        std::fs::write(&path, format!("{line}\n")).unwrap();
        state.poll_and_emit();
        assert_eq!(state.summary.heartbeats, 1);
        assert!(state.last_heartbeat_at.is_some());
    }

    #[test]
    fn heartbeat_rate_limits_consecutive_partials() {
        // Two model.partial events back-to-back (under the 2s window)
        // should produce exactly one heartbeat.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        let lines = "\
            {\"type\":\"model.partial\",\"seq\":1,\"partial_index\":0,\"cumulative_chars\":10}\n\
            {\"type\":\"model.partial\",\"seq\":1,\"partial_index\":1,\"cumulative_chars\":20}\n";
        std::fs::write(&path, lines).unwrap();
        state.poll_and_emit();
        assert_eq!(state.summary.heartbeats, 1, "second partial within window must be coalesced");
    }


    // first_user_symlink_in / is_macos_firmlink tests moved to
    // `darkmux_types::workdir::tests` as part of Wave-E.2 (#255).

    // ─── #590 utility_preflight_warning ───────────────────────────────
    fn lm(identifier: &str, model: &str) -> darkmux_types::LoadedModel {
        darkmux_types::LoadedModel {
            identifier: identifier.into(),
            model: model.into(),
            status: "loaded".into(),
            size: "3 GB".into(),
            context: 4096,
        }
    }

    #[test]
    fn utility_preflight_no_warning_when_loaded() {
        let loaded = vec![lm("darkmux:util-4b", "util-4b"), lm("worker", "worker-35b")];
        // Matched by modelKey...
        assert!(super::utility_preflight_warning("util-4b", &loaded).is_none());
        // ...or by the namespaced identifier.
        assert!(super::utility_preflight_warning("darkmux:util-4b", &loaded).is_none());
    }

    #[test]
    fn utility_preflight_warns_loudly_when_not_loaded() {
        let loaded = vec![lm("worker", "worker-35b")];
        let w = super::utility_preflight_warning("util-4b", &loaded)
            .expect("a registered-but-unloaded util model must warn");
        assert!(w.contains("util-4b"), "names the model: {w}");
        assert!(w.contains("NOT loaded"), "states the problem: {w}");
    }
}
