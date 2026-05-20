//! Internal runtime dispatch path.
//!
//! Routes a `darkmux crew dispatch --runtime internal <role>` invocation
//! to the `darkmux-runtime` docker container instead of openclaw.
//! Per-dispatch container, mounted workspace, structured output collected
//! from stdout.
//!
//! Opt-in via the explicit `--runtime internal` CLI flag while the
//! in-house runtime is being measured against openclaw. Promotion to
//! default is a separate decision tracked in `runtime/README.md`.
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

use crate::crew::dispatch::DispatchResult;
use crate::crew::dispatch::DispatchOpts;
use crate::crew::loader::{load_role_prompt, load_roles};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Docker image tag for the internal runtime. Built locally from
/// `runtime/Dockerfile`. Will become configurable when production
/// hardening lands.
const RUNTIME_IMAGE: &str = "darkmux-runtime:latest";

/// LMStudio /v1/models URL used to probe the currently-loaded model
/// when no explicit model is provided. Currently the internal runtime
/// uses "whatever's loaded"; future iteration will resolve via the
/// role pin table.
const LMSTUDIO_MODELS_URL: &str = "http://localhost:1234/v1/models";

pub fn dispatch(opts: DispatchOpts) -> Result<DispatchResult> {
    eprintln!(
        "darkmux crew dispatch: runtime=internal — image: {RUNTIME_IMAGE}"
    );

    // 1. Load the role manifest + .md prompt. The internal runtime uses
    //    the SAME on-disk role definition as the openclaw path so the
    //    prompts stay identical across runtimes — load-bearing for the
    //    runtime-vs-openclaw comparison.
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

    // 2. Resolve the model. Currently probes LMStudio for whatever's
    //    loaded; future iteration will use the role pin + active profile.
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
    let workspace = match opts.workdir.as_deref() {
        Some(custom) => {
            // Symlink-escape guard (#227 + #232). Walk each operator-typed
            // component and bail if any is a symlink — except for known
            // macOS firmlinks (/tmp, /var, /etc) which operators traverse
            // routinely without realizing they're symlinks.
            //
            // Scope (#232 Issue 2): symlink-only. `..`-traversal paths
            // like /tmp/safe/../../../etc are operator-explicit (typed
            // intentionally) and out of scope — the original threat was
            // an operator surprised by indirection, not by their own
            // deliberate path arithmetic.
            //
            // Why the component-walk over canonicalize-vs-absolute (PR
            // #228 / the canonical-to-canonical pattern proposed in #232):
            // both .canonicalize() calls resolve all symlinks including
            // user-named leaf symlinks, so they always agree on simple
            // path/symlink cases — the comparison silently passes the
            // attack from #227 back through.
            if let Some(offending) = first_user_symlink_in(custom)
                .with_context(|| format!("checking --workdir for symlinks: {}", custom.display()))?
            {
                bail!(
                    "--workdir traverses an operator-named symlink at {} — refusing to follow.\n  \
                     Use the real directory path directly to prevent unintended container r/w.",
                    offending.display()
                );
            }
            let resolved = custom.canonicalize().with_context(|| {
                format!(
                    "--workdir path does not exist or cannot be resolved: {}",
                    custom.display()
                )
            })?;
            if !resolved.is_dir() {
                bail!(
                    "--workdir path is not a directory: {}",
                    resolved.display()
                );
            }
            resolved
        }
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

    // 5. Emit dispatch.start flow record with runtime metadata in payload
    //    (#204). Pairs with dispatch.complete below via session_id, same
    //    as the openclaw path does.
    let dispatch_start_payload = serde_json::json!({
        "runtime": "internal",
        "prompt_chars": opts.message.chars().count(),
        "system_chars": system_prompt.chars().count(),
        "workspace": workspace.display().to_string(),
    });
    let _ = crate::flow::record(crate::crew::dispatch::build_dispatch_record_with_payload(
        crate::flow::Level::Info,
        "dispatch start",
        &opts.role_id,
        &session_id,
        Some(&model),
        Some(dispatch_start_payload),
    ));
    let dispatch_start_instant = std::time::Instant::now();

    // 6. Spawn the docker container. Synchronous; stdout + stderr
    //    captured. The container runs to completion and is removed
    //    automatically (--rm).
    let mut cmd = Command::new("docker");
    cmd.arg("run")
        .arg("--rm")
        .arg("-v")
        .arg(format!("{}:/workspace", workspace.display()))
        .arg(RUNTIME_IMAGE)
        .arg("run")
        .arg("--model")
        .arg(&model)
        .arg("--system")
        .arg(&system_prompt)
        .arg("--prompt")
        .arg(&opts.message);

    let output = cmd
        .output()
        .context("spawning darkmux-runtime container")?;

    let wall_ms = dispatch_start_instant.elapsed().as_millis() as u64;
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // 7. Replay the trajectory file as flow records (#204). Post-hoc — the
    //    runtime writes per-event JSONL to `<workspace>/.darkmux-runtime/
    //    trajectory.jsonl`; we read it after the container exits and
    //    convert each event into the corresponding flow record. Closes
    //    the per-dispatch observability gap that Beat 30 surfaced.
    //    Best-effort: trajectory read failures are non-fatal (the dispatch
    //    succeeded; flow records are observability, not correctness).
    let trajectory_summary = replay_trajectory_to_flow(
        &workspace,
        &session_id,
        &opts.role_id,
        &model,
    );

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
    });
    let (action, level) = if exit_code == 0 {
        ("dispatch complete", crate::flow::Level::Info)
    } else {
        ("dispatch error", crate::flow::Level::Error)
    };
    let _ = crate::flow::record(crate::crew::dispatch::build_dispatch_record_with_payload(
        level,
        action,
        &opts.role_id,
        &session_id,
        Some(&model),
        Some(dispatch_complete_payload),
    ));

    Ok(DispatchResult {
        exit_code,
        stdout,
        stderr,
        session_id,
        watched_state: Vec::new(),
    })
}

/// Summary of what the trajectory replay surfaced. Used to enrich the
/// dispatch.complete payload with end-of-dispatch counts.
#[derive(Default)]
struct TrajectorySummary {
    turns: u32,
    tool_calls: u32,
    compactions: u32,
}

/// Read the runtime's trajectory.jsonl after the dispatch completes and
/// emit per-event flow records: dispatch.turn, dispatch.tool,
/// dispatch.compaction, dispatch.reasoning. Best-effort — any error
/// (file missing, malformed line, write failure) is silently skipped.
/// Returns counts the caller uses to enrich the dispatch.complete record.
fn replay_trajectory_to_flow(
    workspace: &std::path::Path,
    session_id: &str,
    role_id: &str,
    model: &str,
) -> TrajectorySummary {
    use std::io::BufRead;
    let mut summary = TrajectorySummary::default();
    let trajectory = workspace
        .join(".darkmux-runtime")
        .join("trajectory.jsonl");
    let file = match std::fs::File::open(&trajectory) {
        Ok(f) => f,
        Err(_) => return summary, // no trajectory; nothing to replay
    };
    let reader = std::io::BufReader::new(file);
    // `map_while(Result::ok)` stops at the first read error instead of
    // spinning forever on persistent IO errors (clippy::lines_filter_map_ok).
    for line in reader.lines().map_while(Result::ok) {
        let event: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match event_type {
            "model.completed" => {
                summary.turns += 1;
                let payload = serde_json::json!({
                    "turn_seq": event.get("seq"),
                    "finish_reason": event.get("finish_reason"),
                    "tool_calls_count": event.get("tool_calls").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
                    "usage": event.get("usage"),
                });
                let _ = crate::flow::record(crate::crew::dispatch::build_dispatch_record_with_payload(
                    crate::flow::Level::Info,
                    "dispatch.turn",
                    role_id,
                    session_id,
                    Some(model),
                    Some(payload),
                ));
            }
            "tool.completed" => {
                summary.tool_calls += 1;
                let payload = serde_json::json!({
                    "tool_seq": event.get("tool_seq"),
                    "tool_name": event.get("tool_name"),
                    "args_chars": event.get("args_chars"),
                    "result_chars": event.get("result_chars"),
                });
                let _ = crate::flow::record(crate::crew::dispatch::build_dispatch_record_with_payload(
                    crate::flow::Level::Info,
                    "dispatch.tool",
                    role_id,
                    session_id,
                    Some(model),
                    Some(payload),
                ));
            }
            "compaction" => {
                summary.compactions += 1;
                let payload = serde_json::json!({
                    "generation": event.get("generation"),
                    "before_messages": event.get("before_messages"),
                    "after_messages": event.get("after_messages"),
                    "summary_chars": event.get("summary_chars"),
                });
                let _ = crate::flow::record(crate::crew::dispatch::build_dispatch_record_with_payload(
                    crate::flow::Level::Info,
                    "dispatch.compaction",
                    role_id,
                    session_id,
                    Some(model),
                    Some(payload),
                ));
            }
            "model.reasoning" => {
                // The runtime emits these when it parses <think>...</think>
                // blocks from the assistant content (#204). Carries the
                // full reasoning text in payload so the flow viewer can
                // render a collapse/expand block — operator discretion to
                // expand. See runtime/src/loop_runner.rs.
                let payload = serde_json::json!({
                    "turn_seq": event.get("seq"),
                    "reasoning_chars": event.get("reasoning_chars"),
                    "reasoning_text": event.get("reasoning_text"),
                    "reasoning_format": event.get("reasoning_format").unwrap_or(&serde_json::Value::String("inline-think-tags".into())),
                });
                let _ = crate::flow::record(crate::crew::dispatch::build_dispatch_record_with_payload(
                    crate::flow::Level::Info,
                    "dispatch.reasoning",
                    role_id,
                    session_id,
                    Some(model),
                    Some(payload),
                ));
            }
            _ => {
                // Unknown event types (dispatch.start, dispatch.complete
                // from the runtime side) are intentionally ignored — the
                // CLI emits the canonical dispatch.start/complete via
                // build_dispatch_record_with_payload above.
            }
        }
    }
    summary
}

/// Shell out to curl to fetch `/v1/models` from the host's LMStudio and
/// return the first model id. Uses curl so we don't drag a Rust HTTP
/// client dep into darkmux's main crate for one probe call.
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

/// Walk each component of an operator-typed `--workdir` path and return
/// the first symlink encountered that ISN'T a known macOS system
/// firmlink. Returns `Ok(None)` when no operator-named symlink is
/// present along the path. Returns the offending accumulated path so
/// the caller can name it in the error message. (#232)
///
/// The walk stops short (with `Ok(None)`) when a component doesn't
/// exist — the subsequent canonicalize() will surface that as the
/// canonical "does not exist" error.
pub(crate) fn first_user_symlink_in(path: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut acc = PathBuf::new();
    for component in path.components() {
        acc.push(component);
        let meta = match std::fs::symlink_metadata(&acc) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if meta.file_type().is_symlink() && !is_macos_firmlink(&acc) {
            return Ok(Some(acc));
        }
    }
    Ok(None)
}

/// True for the macOS top-level firmlinks operators routinely traverse
/// without thinking. Deliberately narrow: only the three that real
/// `--workdir` paths cross (`/tmp`, `/var`, `/etc`). Other macOS
/// firmlinks (`/Applications`, `/Library`, `/Users`, `/Volumes`, ...)
/// aren't typical workdir destinations; if an operator hits one, the
/// bail is correct behavior. On Linux those paths are real
/// directories, so this never trips. (#232)
pub(crate) fn is_macos_firmlink(p: &Path) -> bool {
    matches!(p.to_str(), Some("/tmp" | "/var" | "/etc"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    // ─── first_user_symlink_in / is_macos_firmlink (#232) ─────────────

    #[test]
    fn first_user_symlink_in_returns_none_for_real_path() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let result = first_user_symlink_in(&real).unwrap();
        assert!(result.is_none(), "non-symlink path must pass");
    }

    #[test]
    fn first_user_symlink_in_detects_leaf_symlink() {
        // The original #227 attack vector: a user-named symlink as the
        // last component of the path. Must be caught.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let sym = tmp.path().join("evilsym");
        symlink(&target, &sym).unwrap();

        let result = first_user_symlink_in(&sym).unwrap();
        assert_eq!(result, Some(sym), "leaf symlink must be detected");
    }

    #[test]
    fn first_user_symlink_in_detects_middle_component_symlink() {
        // A middle-of-path symlink — also catchable.
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(real.join("child")).unwrap();
        let sym = tmp.path().join("sym");
        symlink(&real, &sym).unwrap();
        let probe = sym.join("child");

        let result = first_user_symlink_in(&probe).unwrap();
        assert_eq!(result, Some(sym), "middle-component symlink must be detected");
    }

    #[test]
    fn first_user_symlink_in_returns_none_when_path_does_not_exist() {
        // canonicalize() will surface the not-exists error; the symlink
        // check should not pre-empt it with a spurious result.
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let result = first_user_symlink_in(&missing).unwrap();
        assert!(result.is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn first_user_symlink_in_tolerates_macos_tmp_firmlink() {
        // `/tmp/<real-dir>` traverses the macOS firmlink `/tmp` →
        // `/private/tmp`. The check must NOT bail on this. (#232 Issue 1)
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros();
        let path = std::path::PathBuf::from(format!("/tmp/dm_firmlink_test_{unique}"));
        std::fs::create_dir(&path).unwrap();
        let result = first_user_symlink_in(&path);
        let _ = std::fs::remove_dir(&path);
        assert!(
            result.unwrap().is_none(),
            "/tmp/foo must not trip on macOS firmlink"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn first_user_symlink_in_still_catches_user_symlink_under_tmp() {
        // A user-named symlink one level under /tmp must still be caught
        // even though /tmp itself is a tolerated firmlink. (#232 Issue 1
        // can't be paid for by reopening #227 — both have to hold.)
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros();
        let target = std::path::PathBuf::from(format!("/tmp/dm_target_{unique}"));
        let sym = std::path::PathBuf::from(format!("/tmp/dm_sym_{unique}"));
        std::fs::create_dir(&target).unwrap();
        symlink(&target, &sym).unwrap();

        let result = first_user_symlink_in(&sym);
        let _ = std::fs::remove_file(&sym);
        let _ = std::fs::remove_dir(&target);

        let offending = result.unwrap().expect("user symlink under /tmp must still be caught");
        // The matched component is the symlink itself (under either /tmp
        // or its resolved canonical /private/tmp).
        assert!(
            offending.to_string_lossy().contains(&format!("dm_sym_{unique}")),
            "expected sym in offending path; got {offending:?}"
        );
    }

    #[test]
    fn is_macos_firmlink_allowlist_is_narrow() {
        assert!(is_macos_firmlink(Path::new("/tmp")));
        assert!(is_macos_firmlink(Path::new("/var")));
        assert!(is_macos_firmlink(Path::new("/etc")));
        // Subpaths under firmlinks are NOT tolerated — only the anchors.
        assert!(!is_macos_firmlink(Path::new("/tmp/sub")));
        assert!(!is_macos_firmlink(Path::new("/var/log")));
        assert!(!is_macos_firmlink(Path::new("/")));
        assert!(!is_macos_firmlink(Path::new("/home")));
        assert!(!is_macos_firmlink(Path::new("/Users")));
    }
}
