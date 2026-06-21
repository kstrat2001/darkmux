//! Coding-task workload provider — ports today's lab harness (run-open.sh,
//! run-3.sh, run-probe-recall.sh) to Rust.
//!
//! Setup: copy a sandbox seed into `.darkmux/sandboxes/<workload>/`.
//! Run: dispatch via the active runtime, capture trajectory + reply.
//! Inspect: parse trajectory, identify compactions, classify mode.

use crate::providers::prompt::{extract_reply_text, run_verify};
use darkmux_types::Profile;
use crate::workloads::types::{
    InspectionReport, LoadedWorkload, RunMode, RunResult, WorkloadProvider,
};
use anyhow::{anyhow, bail, Context, Result};
// (#875) `env` is now only used in tests (the default_role read moved to
// config_access); gate the import so the non-test build has no unused-import
// warning under `-D warnings`.
#[cfg(test)]
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) struct CodingTaskProvider;

impl WorkloadProvider for CodingTaskProvider {
    fn id(&self) -> &'static str {
        "coding-task"
    }
    fn description(&self) -> &'static str {
        "Coding workload: prompt + sandbox seed + verification command (e.g. npm test)."
    }

    fn setup(&self, loaded: &LoadedWorkload, run_dir: &Path, sandbox_dir: &Path) -> Result<()> {
        if !run_dir.exists() {
            fs::create_dir_all(run_dir)?;
        }
        if !sandbox_dir.exists() {
            fs::create_dir_all(sandbox_dir)?;
        }

        // 1. Apply external sandbox seed (sibling directory referenced
        //    via `sandboxSeed`). Embedded workloads can't use this
        //    because include_str! only handles individual files.
        if let Some(seed_rel) = manifest_seed_path(loaded) {
            // (#897) The seed path comes from the (operator-installed,
            // untrusted) manifest. Reject absolute / `..` / root components
            // the same way setupContent keys are, then — after joining —
            // canonicalize and assert the resolved path stays under base_dir,
            // so a seed dir that is itself a symlink pointing outside can't
            // pull host content into the agent-visible sandbox.
            reject_escaping_relpath(&seed_rel, "sandboxSeed")?;
            let seed_path = loaded.base_dir.join(&seed_rel);
            if seed_path.exists() {
                let base_canon = loaded.base_dir.canonicalize().with_context(|| {
                    format!("resolving workload dir {}", loaded.base_dir.display())
                })?;
                let seed_canon = seed_path.canonicalize().with_context(|| {
                    format!("resolving sandboxSeed path {}", seed_path.display())
                })?;
                if !seed_canon.starts_with(&base_canon) {
                    bail!(
                        "sandboxSeed `{seed_rel}` resolves outside the workload dir → {}",
                        seed_canon.display()
                    );
                }
                copy_dir_recursive(&seed_canon, sandbox_dir)
                    .with_context(|| format!("seeding sandbox from {}", seed_canon.display()))?;
            }
        }

        // 2. Apply inline setupContent (works with embedded workloads).
        //    Writes each (relative-path → content) pair into the
        //    sandbox dir, creating parent directories as needed.
        //
        //    Precedence: setupContent OVERLAYS on top of sandboxSeed.
        //    If both target the same file, setupContent wins. This lets
        //    an embedded workload patch a specific file over a copied
        //    seed directory.
        //
        //    Re-applies on every dispatch (no skip-if-exists). The
        //    sandbox is operator-mutated by each run (agent edits land
        //    on disk); re-applying setupContent gives every dispatch a
        //    deterministic starting point so re-runs don't measure
        //    cached agent edits as "instant fixes."
        if !loaded.manifest.workload.setup_content.is_empty() {
            for (rel_path, content) in &loaded.manifest.workload.setup_content {
                // Path-traversal hardening: reject absolute paths, `..`
                // components, and any key that would resolve outside
                // sandbox_dir. Embedded workloads are trusted (compiled
                // in), but `~/.darkmux/workloads/<id>.json` is operator-
                // installed and may come from a gist / friend / future
                // install verb. Validate at the receiver, not at install.
                validate_setup_content_key(rel_path).with_context(|| {
                    format!("setupContent key `{rel_path}` is unsafe")
                })?;
                let target = sandbox_dir.join(rel_path);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!(
                            "creating parent dir {} for setupContent file {}",
                            parent.display(),
                            rel_path
                        )
                    })?;
                }
                fs::write(&target, content).with_context(|| {
                    format!("writing setupContent file {}", target.display())
                })?;
            }
            // Operator-visible signal that the sandbox was reset before
            // the agent ran. Without this, an operator looking at a
            // mutated `~/.darkmux/sandboxes/<workload>/` couldn't tell
            // whether the dispatch ran against fresh state or stale
            // post-prior-edit state.
            eprintln!(
                "[lab] setupContent re-applied to {} ({} file(s))",
                sandbox_dir.display(),
                loaded.manifest.workload.setup_content.len()
            );
        }

        // 3. Validate workload+role pairing. Coding-task workloads are
        //    editing-shaped; pairing with a role whose tool_palette
        //    denies both `edit` and `write` (e.g. code-reviewer) gives
        //    the agent a contradiction it can't resolve. Bail at setup
        //    time with an operator-actionable hint — better than
        //    dispatching against a model that will inevitably
        //    "describe the fix without executing it" because the role
        //    forbids the only tools for execution.
        //
        //    The validator silently skips when the role can't be loaded
        //    (unknown id) so the existing dispatch path produces its
        //    own canonical "role not found" error without double-failing.
        let role_id = pick_role(loaded);
        if let Ok(roles) = darkmux_crew::loader::load_roles() {
            if let Some(role) = roles.iter().find(|r| r.id == role_id) {
                if !role_can_modify_files(&role.tool_palette) {
                    bail!(
                        "coding-task workload `{}` is paired with role `{}` whose \
                         tool_palette denies both `edit` and `write` — the agent has no \
                         tools to apply a fix and the dispatch will inevitably waste \
                         wall-clock on a 'describe-but-don't-execute' pattern.\n\
                         \n\
                         Fix one of:\n\
                         \n\
                           1. Set the workload's `role:` field to one that allows edit\n\
                              (e.g. `\"role\": \"coder\"`).\n\
                           2. Unset / override `DARKMUX_DEFAULT_ROLE` if it's pointing\n\
                              at a review-shaped role.\n\
                           3. Use a different workload that's review-shaped (where the\n\
                              prompt asks the agent to FIND issues, not FIX them).",
                        loaded.manifest.workload.id,
                        role_id
                    );
                }
            }
        }

        // 4. Loud-fail when the workload declares requiresExternalSandbox
        //    but the sandbox is empty AND no inline setup content was
        //    applied. Without this check, the dispatch would proceed
        //    against an empty workspace and the agent would hallucinate
        //    files that don't exist — wasting wall-clock for unactionable
        //    output.
        //
        //    (#490) Pre-Phase-3 the operator-actionable hint pointed at
        //    DARKMUX_SANDBOX_<WORKLOAD-ID>. That env-var path is gone.
        //    The hint now points at the fixture-registry path:
        //    `dm lab register` an existing dir, then add a
        //    `requires_fixture` field to the workload manifest.
        if loaded.manifest.workload.requires_external_sandbox
            && loaded.manifest.workload.setup_content.is_empty()
            && sandbox_is_empty(sandbox_dir)
        {
            let requires_hint = loaded
                .manifest
                .workload
                .requires_fixture
                .as_deref()
                .unwrap_or("<satisfies-name>@<version>");
            bail!(
                "workload `{}` requires an external sandbox but `{}` is empty.\n\
                 \n\
                 This workload expects a pre-existing project (e.g. a Node repo with the source\n\
                 files the prompt references). Fix:\n\
                 \n\
                   1. Create <path>/.fixture.json in your project dir with at minimum:\n\
                        {{\"name\": \"<your-name>\", \"satisfies\": \"{}\"}}\n\
                      The `satisfies` value MUST exactly match the workload's `requires_fixture`\n\
                      (resolver does literal-string matching today; semver support tracked in #496).\n\
                 \n\
                   2. Register the project as a fixture:\n\
                        darkmux lab register <path-to-your-project>\n\
                 \n\
                   3. Make sure the workload manifest declares what it needs:\n\
                        \"requires_fixture\": \"{}\"\n\
                 \n\
                   4. Inspect what's registered:\n\
                        darkmux lab fixtures\n\
                 \n\
                 For a coding-task workload that runs out of the box (no external setup), try:\n\
                   darkmux lab run quick-coding",
                loaded.manifest.workload.id,
                sandbox_dir.display(),
                requires_hint,
                requires_hint,
            );
        }

        Ok(())
    }

    fn run(
        &self,
        loaded: &LoadedWorkload,
        run_dir: &Path,
        sandbox_dir: &Path,
        profile: &Profile,
        profile_name: &str,
        runtime: darkmux_crew::dispatch::Runtime,
        runtime_cmd: &str,
        config_path: Option<&str>,
    ) -> Result<RunResult> {
        // (#365/#544) The profile↔loaded envelope check now lives once at
        // the lab-run level (`lab::run` → `profile_check::envelope_warnings`,
        // a superset of the old primary-only check here), so it isn't
        // repeated per provider — a coding-task run warns once, not twice.

        // Per-runtime sandbox-path substitution:
        //   - Openclaw runs on host → agent sees the host sandbox path.
        //   - Internal runtime mounts sandbox_dir at /workspace in the
        //     container → agent sees /workspace. Substituting the host
        //     path here would point the agent at a path invisible
        //     inside Docker (#337 root cause).
        let raw_prompt = resolve_prompt(loaded)?;
        let prompt = match runtime {
            darkmux_crew::dispatch::Runtime::Internal => {
                expand_placeholders_with(&raw_prompt, "/workspace")
            }
            darkmux_crew::dispatch::Runtime::Openclaw => {
                expand_placeholders(&raw_prompt, sandbox_dir)
            }
        };
        let role = pick_role(loaded);
        let session_id = format!(
            "darkmux-coding-{}-{}",
            loaded.manifest.workload.id,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );

        // (#421) Pre-dispatch workspace snapshot. Pure observability:
        // the diff against the post-dispatch snapshot becomes
        // `workspace_delta` in qa-reply.json. Walk failures (oversized
        // workspace, IO errors) are logged but don't fail the dispatch
        // — observability is optional, dispatching the work is not.
        let pre_snapshot = match crate::providers::workspace_delta::compute_snapshot(sandbox_dir) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!(
                    "darkmux: warn — pre-dispatch workspace snapshot skipped: {e}. \
                     `workspace_delta` will be absent from qa-reply.json for this run."
                );
                None
            }
        };

        let started = std::time::Instant::now();
        // `dispatch_out_dir` is the host path where the internal runtime
        // wrote its `.darkmux-runtime/` bookkeeping. `None` for the
        // openclaw path (and pre-image-rebuild internal dispatches) ⇒ the
        // copy site falls back to the legacy sandbox_dir location.
        let mut dispatch_out_dir: Option<PathBuf> = None;
        let (stdout, stderr, ok) = match runtime {
            darkmux_crew::dispatch::Runtime::Internal => {
                // (#368) Derive compaction config from the active
                // profile so the runtime honors operator's
                // profile.runtime.compaction.* without env-var
                // gymnastics. Profile is the operator's tuning
                // source-of-truth; this is the host-side bridge.
                // (#377) Per-role override applied inside
                // dispatch_via_internal where the role manifest is
                // already loaded — single lookup point.
                let compaction =
                    darkmux_crew::dispatch::CompactionDispatchArgs::from_profile(profile);
                // Pass sandbox_dir as --workdir so the runtime mounts
                // it at /workspace, matching the placeholder
                // substitution above (#337 fix).
                let (stdout, stderr, ok, out_dir) = dispatch_via_internal(
                    &role,
                    &prompt,
                    &session_id,
                    Some(sandbox_dir.to_path_buf()),
                    compaction,
                    profile_name,
                    loaded.manifest.workload.image.as_deref(),
                    config_path,
                )?;
                dispatch_out_dir = out_dir;
                (stdout, stderr, ok)
            }
            darkmux_crew::dispatch::Runtime::Openclaw => {
                dispatch_via_openclaw(runtime_cmd, &role, &prompt, &session_id)?
            }
        };
        let duration_ms = started.elapsed().as_millis();

        fs::write(run_dir.join("qa-reply.json"), &stdout)?;
        if !stderr.is_empty() {
            fs::write(run_dir.join("qa-reply.err"), &stderr)?;
        }

        // (#364) Per-run preservation of the runtime's trajectory +
        // metrics.json. The runtime writes both into the out-of-band
        // bookkeeping dir (`<out_dir>/.darkmux-runtime/`) — but that dir
        // is reused across all dispatches of the same workload, so the
        // NEXT dispatch overwrites these files. Copy them into run_dir
        // before that can happen. Phase B dogfood (Beat 39, 2026-05-25)
        // surfaced the methodology gap: per-run aggregator-side
        // analysis was reading the latest dispatch's data for every
        // historical run.
        let mut trajectory_path: Option<PathBuf> = None;
        match runtime {
            darkmux_crew::dispatch::Runtime::Internal => {
                // Read from the dispatch's out-dir. For the internal path
                // `out_dir` is ALWAYS `Some` (the host allocates + mounts it),
                // so the `unwrap_or(sandbox_dir)` fallback only catches the
                // openclaw/remote cases that legitimately have no out-dir.
                // There is NO safe pre-rebuild gap for the internal path: an
                // un-rebuilt image still writes the trajectory into /workspace,
                // which the new tailer never reads, so the #457 watchdog would
                // hard-kill productive dispatches. The host-code change and the
                // darkmux-runtime image rebuild must land atomically.
                let runtime_dir = dispatch_out_dir
                    .as_deref()
                    .unwrap_or(sandbox_dir)
                    .join(".darkmux-runtime");
                // `src.exists()` gate is intentional: a #363-timeout
                // dispatch may have written partial trajectory but no
                // metrics.json. Copying what's there preserves forensic
                // data; missing files just don't copy. Don't "fix" this
                // by aborting when either is absent.
                for (name, dst_name) in [
                    ("trajectory.jsonl", "trajectory.jsonl"),
                    ("metrics.json", "metrics.json"),
                ] {
                    let src = runtime_dir.join(name);
                    if src.exists() {
                        let dst = run_dir.join(dst_name);
                        if let Err(e) = fs::copy(&src, &dst) {
                            eprintln!(
                                "darkmux: warn — failed copying runtime {name} into run dir: {e}"
                            );
                        } else if name == "trajectory.jsonl" {
                            trajectory_path = Some(dst);
                        }
                    }
                }
            }
            darkmux_crew::dispatch::Runtime::Openclaw => {
                // Openclaw writes per-session trajectory under
                // `~/.openclaw/agents/<agent>/sessions/<session-id>.trajectory.jsonl`.
                // Best-effort lookup via guess_trajectory_path.
                if let Some(t) = guess_trajectory_path(&session_id) {
                    let dst = run_dir.join("trajectory.jsonl");
                    if let Err(e) = fs::copy(&t, &dst) {
                        eprintln!("darkmux: warn — failed copying trajectory: {e}");
                    } else {
                        trajectory_path = Some(dst);
                    }
                }
            }
        }

        let verify_outcome = run_verify_command(loaded, run_dir, sandbox_dir)?;

        // (#420) Verify-claim disagreement detection — the "agent
        // thinks it's done; it isn't" failure mode. Compares the
        // agent's final-message claim against verify's outcome and
        // surfaces a structured mismatch when the agent claims
        // completion that verify contradicts. Augments qa-reply.json
        // with `claim_verify_mismatch` so downstream automation can
        // dispatch on the signal without re-parsing verify-output.txt.
        let final_assistant_text = extract_reply_text(&stdout);
        if let Some(mismatch) = detect_claim_verify_mismatch(
            &final_assistant_text,
            verify_outcome.as_ref(),
        ) {
            eprintln!(
                "darkmux: ⚠ verify-claim disagreement detected — agent claimed completion \
                 but verify failed.\n  claim: {}\n  verify: {}",
                mismatch.claim_excerpt, mismatch.verify_details
            );
            // (#420) Best-effort augmentation: discard the Result so a
            // future change inside the helper that introduces an Err
            // path (e.g., a stray `?`) cannot silently turn a dispatch
            // observability augmentation into a dispatch failure.
            // Matches the doctrine documented on the helper itself.
            let _ = augment_qa_reply_with_mismatch(run_dir, &mismatch);
        }

        // (#421) Post-dispatch workspace snapshot + diff. Surfaces
        // `workspace_delta` in qa-reply.json with added / modified /
        // removed paths + total_bytes_changed. Same best-effort
        // discipline as the claim-mismatch augmentation: log + skip
        // on failure; never block the dispatch result.
        if let Some(before) = pre_snapshot.as_ref() {
            match crate::providers::workspace_delta::compute_snapshot(sandbox_dir) {
                Ok(after) => {
                    let delta = crate::providers::workspace_delta::diff_snapshots(before, &after);
                    let _ = augment_qa_reply_with_workspace_delta(run_dir, &delta);
                }
                Err(e) => {
                    eprintln!(
                        "darkmux: warn — post-dispatch workspace snapshot skipped: {e}. \
                         `workspace_delta` will be absent from qa-reply.json for this run."
                    );
                }
            }
        }

        let run_id = run_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        // (#488) Phase 1 — record final_hash of the per-run sandbox so
        // post-hoc analysis can verify "what state did the model leave
        // the sandbox in." Phase 2 will add baseline_hash (the source
        // fixture's hash at clone time) for full provenance.
        // Best-effort: hash failures log a warning but don't abort the
        // dispatch — the data is observability, not correctness.
        let final_hash = match crate::lab::sandbox_hash::hash_sandbox_dir(sandbox_dir) {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!(
                    "darkmux: warn — final_hash for {} skipped: {e}",
                    sandbox_dir.display()
                );
                None
            }
        };
        let manifest_json = serde_json::json!({
            // v2 added: run_id, profile (now the profile NAME), profile_description.
            // v3 (#488) added: final_hash (Phase 1). baseline_hash added in Phase 2.
            "schema_version": 3,
            "run_id": run_id,
            "workload": loaded.manifest.workload.id,
            "provider": self.id(),
            "profile": profile_name,
            "profile_description": profile.description.clone().unwrap_or_default(),
            "duration_ms": duration_ms,
            "ok": ok,
            "session_id": session_id,
            // Always store the sandbox path as absolute in the
            // manifest. Prior to #359 (QA finding), this stored a
            // relative path when sandbox_dir was under cwd — making
            // the manifest non-portable: `darkmux lab inspect`
            // resolving the path against ITS cwd (different from the
            // dispatch cwd) silently failed to find the runtime's
            // metrics.json, falling back to the trajectory-derived
            // counts with turns=0 (the very bug #359 fixes). Always-
            // absolute makes the manifest cwd-independent.
            // (#906 INFO) Best-effort canonicalization: if the sandbox dir
            // can't be canonicalized (rare — e.g. a component vanished mid-run)
            // we fall back to the non-canonical path so the manifest still
            // records *a* path rather than aborting. Degraded provenance in
            // that rare case is acceptable; the absolute-path goal above holds
            // for the normal case.
            "sandbox": sandbox_dir.canonicalize().unwrap_or_else(|_| sandbox_dir.to_path_buf()).display().to_string(),
            "final_hash": final_hash,
        });
        fs::write(
            run_dir.join("manifest.json"),
            serde_json::to_string_pretty(&manifest_json)?,
        )?;

        Ok(RunResult {
            ok,
            duration_ms,
            payload_text: Some(extract_reply_text(&stdout)),
            trajectory_path,
            verify: verify_outcome,
            error: if ok {
                None
            } else {
                Some(format!("runtime exit: {stderr}"))
            },
        })
    }

    fn inspect(&self, loaded: &LoadedWorkload, run_dir: &Path) -> Result<InspectionReport> {
        let manifest_path = run_dir.join("manifest.json");
        let meta = if manifest_path.exists() {
            serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&manifest_path)?)?
        } else {
            serde_json::Value::Null
        };
        let traj_path = run_dir.join("trajectory.jsonl");
        let events = if traj_path.exists() {
            read_jsonl(&traj_path)
        } else {
            Vec::new()
        };

        // Internal-runtime dispatches write a metrics.json next to the
        // trajectory inside the sandbox dir. When present, it's the
        // source-of-truth for turns + compactions — the runtime counts
        // them directly. Trajectory-derived counts (below) work for the
        // openclaw shell-out path which emits `prompt.submitted` events
        // but not for the internal-runtime which emits `model.streaming.
        // start` / `model.completed` and never writes `prompt.submitted`.
        // Pre-fix (#359) the openclaw-shape consumer silently dropped
        // turn counts to 0 on every internal-runtime dispatch.
        //
        // Preference order (after #364):
        //   1. `<run_dir>/metrics.json` — per-run preserved copy. Safe
        //      against subsequent dispatches that would overwrite the
        //      sandbox source.
        //   2. `<sandbox>/.darkmux-runtime/metrics.json` — live source.
        //      Backward-compat for runs predating #364 that don't have
        //      the per-run copy yet. Old-run-only once the runtime writes
        //      out-of-band (#611): new runs no longer leave metrics in the
        //      sandbox, so tier 1 is always authoritative for them.
        let runtime_metrics = read_metrics_json(&run_dir.join("metrics.json")).or_else(|| {
            meta.get("sandbox")
                .and_then(|v| v.as_str())
                .and_then(|sandbox| {
                    read_metrics_json(
                        &Path::new(sandbox).join(".darkmux-runtime").join("metrics.json"),
                    )
                })
        });

        let prompt_submitted: Vec<&serde_json::Value> = events
            .iter()
            .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some("prompt.submitted"))
            .collect();
        let trajectory_turns = prompt_submitted.len() as u32;

        let mut tokens_before: Vec<u64> = Vec::new();
        let mut summary_chars: Vec<u64> = Vec::new();
        let mut seen_summaries = std::collections::HashSet::new();
        for ev in &prompt_submitted {
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
                            tokens_before
                                .push(m.get("tokensBefore").and_then(|v| v.as_u64()).unwrap_or(0));
                            summary_chars.push(summary_str.len() as u64);
                        }
                    }
                }
            }
        }

        let walltime_ms = meta.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0) as u128;
        let mode = classify_mode(walltime_ms, loaded);

        // Prefer runtime metrics when present (internal-runtime path);
        // fall back to trajectory-derived counts (openclaw shell-out
        // path or any other dispatch source).
        // (#371) Reconcile runtime metrics with trajectory-derived
        // counts via `max`. The trajectory is append-only ground truth,
        // so a `metrics.json` that under-reports — absent, partial, or
        // not-yet-finalized when the runtime hard-errors mid-dispatch
        // (SSE timeout / kill / panic) — must never drag the count BELOW
        // what the trajectory actually recorded. Pre-fix, a stale/partial
        // `turns: 0` won over an 84-turn trajectory (Beat 40).
        //
        // Trajectory turns: the openclaw path emits `prompt.submitted`;
        // the internal runtime emits one `model.completed` per turn (and
        // never `prompt.submitted`), so take the max of both shapes.
        // Compactions: `compactionSummary` dedup (openclaw) vs
        // `compaction` events (internal runtime).
        let trajectory_turns = trajectory_turns.max(count_event_type(&events, "model.completed"));
        let trajectory_compactions =
            (tokens_before.len() as u32).max(count_event_type(&events, "compaction"));
        let turns = reconcile_count(runtime_metrics.as_ref().and_then(|m| m.turns), trajectory_turns);
        let compactions = reconcile_count(
            runtime_metrics.as_ref().and_then(|m| m.compactions),
            trajectory_compactions,
        );
        let mut notes = vec![
            format!("turns={}", turns),
            format!("compactions={}", compactions),
            format!("walltime={}s", walltime_ms / 1000),
        ];
        if let Some(m) = mode {
            notes.push(format!(
                "mode={}",
                match m {
                    RunMode::Fast => "fast",
                    RunMode::Slow => "slow",
                }
            ));
        }
        let verify = run_verify(
            loaded,
            &extract_reply_text(
                &fs::read_to_string(run_dir.join("qa-reply.json")).unwrap_or_default(),
            ),
        );
        notes.push(format!(
            "verify: {}",
            if verify.passed { "ok" } else { "fail" }
        ));

        let run_id = meta
            .get("run_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                run_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "(unknown)".to_string());
        Ok(InspectionReport {
            run_id,
            workload_id: loaded.manifest.workload.id.clone(),
            walltime_ms,
            turns,
            compactions,
            tokens_before,
            summary_chars,
            mode,
            notes,
        })
    }
}

/// Substitute `${SANDBOX_DIR}` and `${SANDBOX}` in a workload prompt or
/// command with the resolved sandbox path. This is what lets a workload
/// manifest be portable — the manifest references `${SANDBOX_DIR}` and the
/// runtime supplies the actual on-disk path (which may come from the
/// `DARKMUX_SANDBOX_<workload>` env var).
///
/// For prompts dispatched through the internal Docker-bounded runtime
/// (where `sandbox_dir` is mounted at `/workspace` inside the container —
/// see `src/crew/dispatch_internal.rs:99`), callers should use
/// `expand_placeholders_with("/workspace")` instead. Otherwise the agent
/// reads the prompt's host path, can't find files at that path inside
/// the container, and produces an empty trajectory. Verify commands
/// run on the host and continue to use the host path.
fn expand_placeholders(input: &str, sandbox_dir: &Path) -> String {
    expand_placeholders_with(input, &sandbox_dir.display().to_string())
}

/// Lower-level helper: substitute `${SANDBOX_DIR}` / `${SANDBOX}` with
/// an explicit view-path string. Used to swap between host paths
/// (openclaw / verify command) and container-internal paths (internal
/// runtime agent prompts) at the call site.
fn expand_placeholders_with(input: &str, view_path: &str) -> String {
    input
        .replace("${SANDBOX_DIR}", view_path)
        .replace("${SANDBOX}", view_path)
}

fn manifest_seed_path(loaded: &LoadedWorkload) -> Option<String> {
    if let Some(s) = loaded.manifest.workload.sandbox_seed.as_ref() {
        return Some(s.clone());
    }
    Some("sandbox".to_string())
}

/// True when `path` either doesn't exist, isn't a directory, or is an
/// empty directory. Used by the requires-external-sandbox loud-fail
/// check to distinguish "operator set up the sandbox" from "operator
/// expected the workload to magically work."
fn sandbox_is_empty(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut iter) => iter.next().is_none(),
        Err(_) => true,
    }
}

/// True when the role's tool_palette permits either `edit` or `write`
/// (after applying the deny list). Coding-task workloads are editing-
/// shaped by definition; if neither tool is available the agent can't
/// satisfy the prompt no matter how good the model is.
///
/// Catches the methodology bug from 2026-05-24: pairing a coding-task
/// workload with `code-reviewer` (whose tool_palette denies edit AND
/// write) gives the agent a contradiction it can't resolve. Better to
/// bail at setup time with an operator-actionable hint than to dispatch
/// against a model that will inevitably "describe the fix without
/// executing it" because it has no tools for execution.
fn role_can_modify_files(palette: &darkmux_crew::types::ToolPalette) -> bool {
    let allows = |name: &str| {
        palette.allow.iter().any(|a| a == name)
            && !palette.deny.iter().any(|d| d == name)
    };
    allows("edit") || allows("write")
}

/// Reject setupContent keys that would write outside the sandbox dir.
///
/// Three classes of attack-shape this catches:
///   1. **Absolute paths**: `PathBuf::from(sandbox).join("/etc/passwd")`
///      returns `/etc/passwd` — `Path::join` silently replaces when the
///      arg is absolute. A workload manifest's setupContent key of
///      `"/etc/cron.d/evil"` would write to system state.
///   2. **Parent traversal**: `"../../etc/shadow"` walks out of the
///      sandbox via `create_dir_all` + `write`.
///   3. **Windows drive letters / UNC prefixes**: same risk class as
///      absolute paths on Windows. Reject defensively even though
///      darkmux is Apple-Silicon-tested.
///
/// Embedded workloads (compiled-in JSON via `include_str!`) are trusted
/// by construction. Operator-installed workloads under
/// `~/.darkmux/workloads/<id>.json` may come from anywhere; validate at
/// the consumer rather than at install. The cost is tiny and the
/// invariant is much stronger.
fn validate_setup_content_key(key: &str) -> Result<()> {
    reject_escaping_relpath(key, "setupContent key")
}

/// Reject a relative path that would escape the sandbox dir — absolute
/// paths, `..` traversal, or a root/prefix component. Shared by
/// `validate_setup_content_key` and the `sandboxSeed` check (#897) so the
/// same trust boundary is enforced identically in both places — one place
/// to audit the escape rule.
fn reject_escaping_relpath(raw: &str, label: &str) -> Result<()> {
    if raw.is_empty() {
        bail!("{label} is empty");
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        bail!(
            "{label} `{raw}` is an absolute path; only relative paths under the sandbox are allowed"
        );
    }
    // (#906) Track whether the key names an actual file component. A key of
    // `.` / `./` / `./.` normalizes to only `CurDir` components — it passes
    // the escape check but points AT the sandbox dir, so the later `fs::write`
    // fails opaquely with `IsADirectory`. Reject it here with a clear message.
    let mut has_normal = false;
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::Normal(_) => has_normal = true,
            Component::CurDir => continue,
            Component::ParentDir => {
                bail!("{label} `{raw}` contains `..` — would escape the sandbox")
            }
            Component::Prefix(_) | Component::RootDir => bail!(
                "{label} `{raw}` contains a root/prefix component — only relative paths under the sandbox are allowed"
            ),
        }
    }
    if !has_normal {
        bail!("{label} `{raw}` names no file under the sandbox (resolves to the sandbox dir itself)");
    }
    Ok(())
}

fn resolve_prompt(loaded: &LoadedWorkload) -> Result<String> {
    if let Some(p) = loaded.manifest.workload.prompt.as_ref() {
        return Ok(p.clone());
    }
    if let Some(rel) = loaded.manifest.workload.prompt_file.as_ref() {
        let path = loaded.base_dir.join(rel);
        return fs::read_to_string(&path)
            .with_context(|| format!("reading promptFile at {}", path.display()));
    }
    Err(anyhow!(
        "coding-task workload \"{}\" must define prompt or promptFile",
        loaded.manifest.workload.id
    ))
}

/// Resolve which darkmux role to dispatch the coding-task workload
/// through. Workloads reference DM role manifest ids, not OC agent
/// personas. Default: `coder` — coding-task workloads ARE editing-shaped
/// by definition (the agent reads code + modifies it + runs tests).
/// The earlier default of `code-reviewer` was a methodology bug
/// surfaced 2026-05-24: code-reviewer's tool_palette denies edit/write,
/// so any coding-task workload that defaulted to it would hand the
/// agent a contradiction (prompt: "fix the bug"; doctrine: "you must
/// not modify code"). Override via the workload's `role:` field or
/// `DARKMUX_DEFAULT_ROLE` env var.
fn pick_role(loaded: &LoadedWorkload) -> String {
    if let Some(r) = loaded.manifest.workload.role.as_deref() {
        return r.to_string();
    }
    // (#875) env > config.runtime.default_role > "coder", via config_access.
    darkmux_types::config_access::default_role().unwrap_or_else(|| "coder".to_string())
}

/// Dispatch via darkmux's internal Docker-bounded runtime through the
/// crew::dispatch substrate. `workdir` (when `Some`) passes as
/// `--workdir` so the runtime mounts the host path at `/workspace`
/// inside the container, giving the agent access to the workload's
/// sandbox files (#337 fix).
#[allow(clippy::too_many_arguments)]
fn dispatch_via_internal(
    role_id: &str,
    prompt: &str,
    session_id: &str,
    workdir: Option<PathBuf>,
    compaction: darkmux_crew::dispatch::CompactionDispatchArgs,
    profile_name: &str,
    image: Option<&str>,
    config_path: Option<&str>,
) -> Result<(String, String, bool, Option<PathBuf>)> {
    use darkmux_crew::dispatch::{dispatch, DispatchOpts, Runtime};
    let opts = DispatchOpts {
        role_id: role_id.to_string(),
        message: prompt.to_string(),
        deliver: None,
        session_id: Some(session_id.to_string()),
        timeout_seconds: 3600,
        skip_preflight: false,
        json: true,
        watch_paths: Vec::new(),
        workdir,
        sprint_id: None,
        runtime: Runtime::Internal,
        runtime_cmd: "openclaw".to_string(),
        machine: None,
        wait: true,
        compaction,
        // (#549) Lab runs resolve + log against the run's resolved
        // profile (the CLI `--profile` override when set), not the
        // registry default.
        profile_name: Some(profile_name.to_string()),
        // (#984) Propagate the lab `--profiles-file` so the dispatch's model
        // resolution loads from it (not just lab run's own profile lookup).
        config_path: config_path.map(str::to_string),
        // (#703 Slice 4) the workload's declared image (manifest
        // `workload.image`), injected so the agent can build/test in-sandbox.
        image: image.map(str::to_string),
    };
    let result = dispatch(opts).context("internal-runtime dispatch via lab harness")?;
    // `out_dir` is the host path where the runtime wrote its
    // `.darkmux-runtime/` bookkeeping (trajectory + metrics). Threaded
    // back to the copy-into-run_dir site. `None` pre-image-rebuild ⇒
    // caller falls back to the legacy sandbox_dir location.
    Ok((
        result.stdout,
        result.stderr,
        result.exit_code == 0,
        result.out_dir,
    ))
}

/// Dispatch via the legacy openclaw shell-out path. Shells out with the
/// `<cmd> agent --agent <role> --json ...` calling convention.
/// `runtime_cmd` is the operator-supplied binary path (Sprint-E:
/// `--runtime-cmd <path>` flag; defaults to `"openclaw"`).
fn dispatch_via_openclaw(
    runtime_cmd: &str,
    role: &str,
    prompt: &str,
    session_id: &str,
) -> Result<(String, String, bool)> {
    let output = Command::new(runtime_cmd)
        .args([
            "agent",
            "--agent",
            role,
            "--session-id",
            session_id,
            "--json",
            "--timeout",
            "3600",
            "--message",
            prompt,
        ])
        .output()
        .with_context(|| format!("running `{runtime_cmd} agent ...`"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Ok((stdout, stderr, output.status.success()))
}

/// Best-effort trajectory lookup for the active runtime.
///
/// darkmux doesn't know where every agent runtime stores trajectories, so we
/// look in the location the OpenClaw runtime uses by default. Override with
/// `DARKMUX_RUNTIME_AGENTS_DIR` for any other runtime that stores per-agent
/// session files in a parallel layout: `<dir>/<agent>/sessions/<session-id>.trajectory.jsonl`.
fn guess_trajectory_path(session_id: &str) -> Option<PathBuf> {
    // env(DARKMUX_RUNTIME_AGENTS_DIR) > config.dirs.runtime_agents >
    // ~/.openclaw/agents (None if no HOME and no override) (#661 Slice 3).
    let agents_dir = match darkmux_types::config_access::runtime_agents_dir_override() {
        Some(p) => p,
        None => dirs::home_dir()?.join(".openclaw").join("agents"),
    };
    if !agents_dir.exists() {
        return None;
    }
    let entries = fs::read_dir(&agents_dir).ok()?;
    for entry in entries.flatten() {
        let candidate = entry
            .path()
            .join("sessions")
            .join(format!("{session_id}.trajectory.jsonl"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// (#420) Result of comparing the agent's final-message claim against
/// the verify command's outcome. Surfaces the "agent thinks it's
/// done; it isn't" failure mode — worse than a silent bail because
/// the dispatch succeeds *misleadingly* (final_assistant looks
/// substantive, downstream automation takes the agent's claim as
/// canonical, the verify-output.txt failure goes uninspected).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ClaimVerifyMismatch {
    /// The phrase from final_assistant that triggered the claim
    /// detector (truncated for compactness in qa-reply.json).
    pub claim_excerpt: String,
    /// Verify's `details` field at the time of detection. Stored so
    /// post-hoc analysis sees both sides of the disagreement
    /// together.
    pub verify_details: String,
}

/// (#420) Pure detector: given the agent's final message + the
/// verify outcome, decide whether the agent claimed completion in a
/// way that contradicts what verify observed.
///
/// Returns `Some(ClaimVerifyMismatch)` only when ALL of:
/// - verify ran and reported `passed: false`
/// - final_assistant contains a positive-claim pattern (*"tests
///   pass"*, *"successfully completed"*, *"no failures"*, etc.)
/// - final_assistant does NOT contain an honesty marker that would
///   override the positive claim (*"still failing"*, *"remaining"*,
///   *"partial"*, etc.)
///
/// Conservative by design: false positives (flagging clean dispatches
/// as mismatches) erode trust faster than missing real disagreements.
/// The patterns start narrow; operators can broaden empirically as
/// more failure traces accumulate.
/// Largest char boundary `<= i` in `s`. (#869) Keeps byte-window slicing
/// panic-safe on non-ASCII without depending on the recently-stabilized
/// `str::floor_char_boundary`.
fn floor_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut i = i;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub(crate) fn detect_claim_verify_mismatch(
    final_assistant: &str,
    verify: Option<&crate::workloads::types::VerifyOutcome>,
) -> Option<ClaimVerifyMismatch> {
    let v = verify?;
    if v.passed {
        return None;
    }
    let lowered = final_assistant.to_lowercase();

    // Honesty markers override any positive claim — agent
    // acknowledging the failure means no disagreement.
    const HONESTY_MARKERS: &[&str] = &[
        "still failing",
        "still fails",
        "failing test",
        "failed test",
        "tests still",
        "remaining issue",
        "remaining failure",
        "remaining test",
        "tests remain",
        "partial",
        "incomplete",
        "not done",
        "unfinished",
        "blocked",
        "could not complete",
        "couldn't complete",
        "not complete",
    ];
    for m in HONESTY_MARKERS {
        if lowered.contains(m) {
            return None;
        }
    }

    // Positive-claim patterns. Conservative starting set.
    const POSITIVE_CLAIMS: &[&str] = &[
        "all tests pass",
        "all tests passed",
        "all tests passing",
        "all green",
        "successfully completed",
        "completed successfully",
        "no failures",
        "no errors",
        "no remaining issues",
        "no remaining issues remain",
        "everything works",
        "everything passes",
        "everything green",
    ];
    let mut matched: Option<&str> = None;
    for c in POSITIVE_CLAIMS {
        if lowered.contains(c) {
            matched = Some(c);
            break;
        }
    }
    let claim = matched?;

    // Build an excerpt centered on the matched claim for forensic
    // visibility. ~80 chars on either side, clipped at message
    // boundaries.
    let idx = lowered.find(claim).expect("claim was just matched");
    // (#869) Window the excerpt in `lowered`'s OWN index space. `idx` is a byte
    // offset into `lowered` (the to_lowercase() copy), which is NOT offset-
    // compatible with `final_assistant` on non-ASCII text (e.g. `İ` → `i̇`
    // changes byte length) — slicing the original with these indices could
    // split a multibyte codepoint and panic. The excerpt is forensic context,
    // so the lowercased copy is fine. The ±80 window can itself land mid-
    // codepoint, so clamp both ends to char boundaries before slicing.
    let start = floor_char_boundary(&lowered, idx.saturating_sub(80));
    let end = floor_char_boundary(&lowered, (idx + claim.len() + 80).min(lowered.len()));
    let excerpt = lowered[start..end].trim().to_string();

    Some(ClaimVerifyMismatch {
        claim_excerpt: excerpt,
        verify_details: v.details.clone(),
    })
}

/// (#420) Augment the runtime's qa-reply.json with the detected
/// claim-verify mismatch. Parse-add-write rather than touching the
/// runtime crate: the runtime owns the envelope shape and shouldn't
/// know about the host's post-dispatch verification. Operator-facing
/// shape:
///
/// ```json
/// {
///   "final_assistant": "...",
///   "result": "stop",
///   ...,
///   "claim_verify_mismatch": {
///     "claim_excerpt": "...",
///     "verify_details": "exit 1"
///   }
/// }
/// ```
///
/// Best-effort: a parse or write failure logs a warning but does NOT
/// fail the dispatch — observability augmentation isn't worth
/// aborting the result over.
fn augment_qa_reply_with_mismatch(
    run_dir: &Path,
    mismatch: &ClaimVerifyMismatch,
) -> Result<()> {
    let path = run_dir.join("qa-reply.json");
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("darkmux: warn — couldn't read qa-reply.json to augment: {e}");
            return Ok(());
        }
    };
    let mut parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("darkmux: warn — couldn't parse qa-reply.json to augment: {e}");
            return Ok(());
        }
    };
    if let Some(obj) = parsed.as_object_mut() {
        obj.insert(
            "claim_verify_mismatch".to_string(),
            serde_json::json!({
                "claim_excerpt": mismatch.claim_excerpt,
                "verify_details": mismatch.verify_details,
            }),
        );
        if let Ok(reserialized) = serde_json::to_string(&parsed) {
            if let Err(e) = fs::write(&path, reserialized) {
                eprintln!("darkmux: warn — couldn't write augmented qa-reply.json: {e}");
            }
        }
    }
    Ok(())
}

/// (#421) Augment the runtime's qa-reply.json with the workspace
/// delta. Mirrors the structure of `augment_qa_reply_with_mismatch`
/// (#420): parse-add-write, best-effort, log + skip on failure.
///
/// Operator-facing shape:
///
/// ```json
/// {
///   ...,
///   "workspace_delta": {
///     "added":   ["tests/foo.test.ts"],
///     "modified": ["src/services/refreshTokenService.ts"],
///     "removed": [],
///     "total_bytes_changed": 1240
///   }
/// }
/// ```
///
/// Path lists are sorted (BTreeMap iteration in the diff) so two
/// runs on the same inputs produce byte-identical JSON.
fn augment_qa_reply_with_workspace_delta(
    run_dir: &Path,
    delta: &crate::providers::workspace_delta::WorkspaceDelta,
) -> Result<()> {
    let path = run_dir.join("qa-reply.json");
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("darkmux: warn — couldn't read qa-reply.json to augment with delta: {e}");
            return Ok(());
        }
    };
    let mut parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("darkmux: warn — couldn't parse qa-reply.json to augment with delta: {e}");
            return Ok(());
        }
    };
    if let Some(obj) = parsed.as_object_mut() {
        let added: Vec<String> = delta
            .added
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let modified: Vec<String> = delta
            .modified
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let removed: Vec<String> = delta
            .removed
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        obj.insert(
            "workspace_delta".to_string(),
            serde_json::json!({
                "added": added,
                "modified": modified,
                "removed": removed,
                "total_bytes_changed": delta.total_bytes_changed,
            }),
        );
        if let Ok(reserialized) = serde_json::to_string(&parsed) {
            if let Err(e) = fs::write(&path, reserialized) {
                eprintln!("darkmux: warn — couldn't write delta-augmented qa-reply.json: {e}");
            }
        }
    }
    Ok(())
}

/// Run the workload's verify command and capture its outcome.
///
/// SECURITY (#906): the command runs on the HOST shell (`/bin/sh -c`) with
/// the operator's privileges — NOT inside the dispatch container that bounds
/// the agent. A workload/fixture from an untrusted source could carry a
/// hostile `verify.command`. `darkmux lab register` warns at register time
/// when a fixture declares one (the trust-establishment point); running
/// verify inside the declared `--image` is a deferred hardening step.
fn run_verify_command(
    loaded: &LoadedWorkload,
    run_dir: &Path,
    sandbox_dir: &Path,
) -> Result<Option<crate::workloads::types::VerifyOutcome>> {
    let Some(spec) = loaded.manifest.workload.verify.as_ref() else {
        return Ok(None);
    };
    let Some(cmd_raw) = spec.command.as_ref() else {
        return Ok(None);
    };
    // Same ${SANDBOX_DIR} substitution as the prompt — verify commands
    // commonly need to cd into the sandbox or reference files there.
    let cmd = expand_placeholders(cmd_raw, sandbox_dir);
    let cwd = if let Some(rel) = spec.cwd.as_ref() {
        sandbox_dir.join(expand_placeholders(rel, sandbox_dir))
    } else {
        sandbox_dir.to_path_buf()
    };
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(&cmd)
        .current_dir(&cwd)
        .output()
        .with_context(|| format!("running verify command: {cmd}"))?;
    let merged = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(run_dir.join("verify-output.txt"), &merged)?;
    Ok(Some(crate::workloads::types::VerifyOutcome {
        passed: output.status.success(),
        details: if output.status.success() {
            "verify command exited 0".into()
        } else {
            format!("exit {}", output.status.code().unwrap_or(-1))
        },
    }))
}

fn classify_mode(walltime_ms: u128, loaded: &LoadedWorkload) -> Option<RunMode> {
    let ex = loaded.manifest.workload.expected.as_ref()?;
    let seconds = (walltime_ms / 1000) as u64;
    if let Some((_, hi)) = ex.fast_cluster_seconds {
        if seconds <= hi {
            return Some(RunMode::Fast);
        }
    }
    if let Some((lo, _)) = ex.slow_cluster_seconds {
        if seconds >= lo {
            return Some(RunMode::Slow);
        }
    }
    None
}

/// Snapshot of the internal runtime's per-dispatch metrics.json
/// (written by `runtime/src/main.rs` next to trajectory.jsonl inside
/// the sandbox dir). Only the fields the lab inspect surface consumes
/// — the runtime writes more (model id, version, finish reason, etc.)
/// but inspect doesn't need them.
///
/// Optional fields use `None` rather than `0` to discriminate
/// "runtime didn't report this" from "runtime reported zero." Lets
/// the consumer prefer runtime data only when it's actually present.
#[derive(Debug, Clone, Default)]
struct InternalRuntimeMetrics {
    turns: Option<u32>,
    compactions: Option<u32>,
}

/// Read a runtime-emitted `metrics.json` from a specific path.
/// Returns `None` if the file doesn't exist (openclaw shell-out
/// dispatches won't have one; some run dirs don't yet have the
/// per-run copy from #364) or if it can't be parsed (older runtime
/// versions may emit a different shape). The fallback in the caller
/// is to derive counts from the trajectory (#359).
///
/// Caller-chooses the path so the preference chain (per-run copy
/// first, sandbox-live fallback) lives at the consumer, not split
/// across multiple helpers.
/// Count trajectory events of a given `"type"`. (#371)
fn count_event_type(events: &[serde_json::Value], ty: &str) -> u32 {
    events
        .iter()
        .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some(ty))
        .count() as u32
}

/// (#371) Reconcile a runtime-reported count with the trajectory-derived
/// count. The trajectory is append-only ground truth, so the metric can
/// never legitimately be LOWER than what the trajectory recorded — a
/// missing (`None`) or stale/partial metric (e.g. `turns: 0` written
/// before a hard-error exit) is corrected upward to the trajectory
/// count. When the metric is complete it wins (and ties are a no-op);
/// when the trajectory itself is truncated, the higher metric is kept.
fn reconcile_count(metric: Option<u32>, trajectory: u32) -> u32 {
    metric.unwrap_or(0).max(trajectory)
}

fn read_metrics_json(path: &Path) -> Option<InternalRuntimeMetrics> {
    let raw = fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(InternalRuntimeMetrics {
        turns: v.get("turns").and_then(|x| x.as_u64()).map(|n| n as u32),
        compactions: v
            .get("compactions")
            .and_then(|x| x.as_u64())
            .map(|n| n as u32),
    })
}

fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    if !dst.exists() {
        fs::create_dir_all(dst)?;
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        // (#897) No-follow: `file_type()` comes from the directory entry
        // (lstat-like), so a symlink reports `is_symlink()` rather than its
        // target's type. SKIP symlinks — copying or recreating a seed symlink
        // (→ /etc, ~/.ssh, …) would pull host content into the agent-visible
        // sandbox. Only real files/dirs are seeded.
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_recursive(&s, &d)?;
        } else {
            fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn _unused_bail() -> Result<()> {
    bail!("unused")
}

// Note: a small `pathdiff` mod used to live here for computing
// relative paths in the manifest. Removed in #359 when the manifest
// switched to always-absolute sandbox paths (the relative form was
// the root cause of inspect resolving against the wrong cwd).

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workloads::types::{
        ExpectedSpec, VerifySpec, WorkloadManifest, WorkloadSource, WorkloadSpec,
    };
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    // ── (#371) metrics ↔ trajectory reconciliation ──────────────────

    #[test]
    fn reconcile_count_corrects_stale_metric_upward() {
        // The Beat-40 case: a stale/partial metrics.json says turns=0
        // but the trajectory recorded 84 model.completed events.
        assert_eq!(reconcile_count(Some(0), 84), 84);
    }

    #[test]
    fn reconcile_count_falls_back_to_trajectory_when_metric_absent() {
        // Hard error before metrics.json was written at all.
        assert_eq!(reconcile_count(None, 84), 84);
    }

    #[test]
    fn reconcile_count_keeps_complete_metric() {
        // Metrics complete and at least the trajectory count: metric wins
        // (covers a truncated trajectory where the metric is higher).
        assert_eq!(reconcile_count(Some(90), 84), 90);
        assert_eq!(reconcile_count(Some(84), 84), 84);
    }

    #[test]
    fn reconcile_count_zero_when_neither_has_data() {
        assert_eq!(reconcile_count(None, 0), 0);
    }

    #[test]
    fn count_event_type_counts_internal_runtime_turns() {
        let events: Vec<serde_json::Value> = vec![
            serde_json::json!({"type": "dispatch.start"}),
            serde_json::json!({"type": "model.completed"}),
            serde_json::json!({"type": "tool.completed"}),
            serde_json::json!({"type": "model.completed"}),
            serde_json::json!({"type": "compaction"}),
            serde_json::json!({"type": "model.completed"}),
        ];
        assert_eq!(count_event_type(&events, "model.completed"), 3);
        assert_eq!(count_event_type(&events, "compaction"), 1);
        assert_eq!(count_event_type(&events, "nonexistent"), 0);
    }

    fn make_loaded(spec: WorkloadSpec, base_dir: PathBuf) -> LoadedWorkload {
        LoadedWorkload {
            manifest: WorkloadManifest { workload: spec },
            manifest_path: base_dir.join("workload.json"),
            base_dir,
            source: WorkloadSource::Builtin,
        }
    }

    fn basic_spec() -> WorkloadSpec {
        WorkloadSpec {
            id: "coding".into(),
            provider: "coding-task".into(),
            description: None,
            role: None,
            prompt: Some("write tests".into()),
            prompt_file: None,
            sandbox_seed: None,
            setup_content: BTreeMap::new(),
            requires_external_sandbox: false,
            requires_fixture: None,
            verify: None,
            expected: None,
            image: None,
            extras: BTreeMap::new(),
        }
    }

    #[test]
    fn provider_metadata() {
        let p = CodingTaskProvider;
        assert_eq!(p.id(), "coding-task");
        assert!(p.description().contains("sandbox"));
    }

    #[test]
    fn manifest_seed_path_default_is_sandbox() {
        let tmp = TempDir::new().unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        assert_eq!(manifest_seed_path(&loaded), Some("sandbox".to_string()));
    }

    #[test]
    fn manifest_seed_path_uses_explicit() {
        let tmp = TempDir::new().unwrap();
        let mut spec = basic_spec();
        spec.sandbox_seed = Some("custom-seed".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        assert_eq!(manifest_seed_path(&loaded), Some("custom-seed".into()));
    }

    /// Coding-task default role is `coder` (allows edit) — NOT
    /// code-reviewer. The earlier default of `code-reviewer` was the
    /// methodology bug from 2026-05-24: coding-task workloads are
    /// editing-shaped by definition; pairing them with a role whose
    /// tool_palette denies edit produces a contradiction the agent
    /// can't satisfy. Regression guard.
    #[test]
    #[serial_test::serial]
    fn pick_role_default_coder() {
        let tmp = TempDir::new().unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        unsafe { env::remove_var("DARKMUX_DEFAULT_ROLE") };
        assert_eq!(pick_role(&loaded), "coder");
    }

    // ─── role_can_modify_files (workload-role contradiction validator) ──

    fn palette(allow: &[&str], deny: &[&str]) -> darkmux_crew::types::ToolPalette {
        darkmux_crew::types::ToolPalette {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn role_can_modify_coder_palette_yes() {
        // coder: allow [read, edit, write, exec, process], deny []
        let p = palette(&["read", "edit", "write", "exec", "process"], &[]);
        assert!(role_can_modify_files(&p));
    }

    #[test]
    fn role_can_modify_code_reviewer_palette_no() {
        // code-reviewer: allow [read, exec, update_plan], deny [edit, write, process]
        // This is the regression guard for the 2026-05-24 methodology bug.
        let p = palette(
            &["read", "exec", "update_plan"],
            &["edit", "write", "process"],
        );
        assert!(
            !role_can_modify_files(&p),
            "code-reviewer must NOT be classified as able to modify files"
        );
    }

    #[test]
    fn role_can_modify_empty_palette_no() {
        let p = palette(&[], &[]);
        assert!(!role_can_modify_files(&p));
    }

    #[test]
    fn role_can_modify_deny_overrides_allow() {
        let p = palette(&["edit", "write"], &["edit", "write"]);
        assert!(!role_can_modify_files(&p));
    }

    #[test]
    fn role_can_modify_only_write_allowed_yes() {
        let p = palette(&["read", "write"], &[]);
        assert!(role_can_modify_files(&p));
    }

    #[test]
    fn role_can_modify_only_edit_allowed_yes() {
        let p = palette(&["read", "edit"], &[]);
        assert!(role_can_modify_files(&p));
    }

    /// Integration: pairing a coding-task workload with `code-reviewer`
    /// (the methodology bug class) bails at setup() with the
    /// operator-actionable hint. Uses the embedded role manifests so
    /// the test doesn't depend on user state.
    #[test]
    #[serial_test::serial]
    fn setup_bails_when_workload_role_is_code_reviewer() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let mut spec = basic_spec();
        spec.role = Some("code-reviewer".to_string());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());

        let err = CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .expect_err("setup must bail when role denies modification");
        let msg = err.to_string();
        assert!(
            msg.contains("code-reviewer"),
            "expected error to name the offending role; got: {msg}"
        );
        assert!(
            msg.contains("`edit`") && msg.contains("`write`"),
            "expected error to name the missing tools; got: {msg}"
        );
        assert!(
            msg.contains("coder"),
            "expected error to point at `coder` as the fix; got: {msg}"
        );
    }

    /// Integration regression: the default role (`coder`, post-2026-05-24
    /// fix) passes the validator and setup() proceeds without bail.
    #[test]
    #[serial_test::serial]
    fn setup_does_not_bail_when_default_role_is_coder() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        // basic_spec has role=None → pick_role defaults to "coder".
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        unsafe { env::remove_var("DARKMUX_DEFAULT_ROLE") };

        CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .expect("default coder role must pass validation");
    }

    #[test]
    fn pick_role_from_manifest() {
        let tmp = TempDir::new().unwrap();
        let mut spec = basic_spec();
        spec.role = Some("analyst".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        assert_eq!(pick_role(&loaded), "analyst");
    }

    #[test]
    fn copy_dir_recursive_works() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("a.txt"), "alpha").unwrap();
        fs::write(src.join("nested/b.txt"), "bravo").unwrap();
        copy_dir_recursive(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "alpha");
        assert_eq!(
            fs::read_to_string(dst.join("nested/b.txt")).unwrap(),
            "bravo"
        );
    }

    #[test]
    fn copy_dir_recursive_creates_dst() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("not-yet-there/dst");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("x.txt"), "x").unwrap();
        copy_dir_recursive(&src, &dst).unwrap();
        assert!(dst.exists());
        assert_eq!(fs::read_to_string(dst.join("x.txt")).unwrap(), "x");
    }

    #[test]
    fn copy_dir_recursive_skips_symlinks() {
        // (#897) A symlink in the seed tree must NOT be copied/followed —
        // otherwise a seed symlink → host content (/etc, ~/.ssh) would land
        // in the agent-visible sandbox. Pre-fix `fs::copy` followed the link
        // and copied the host file's content as a regular file.
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("host-secret.txt");
        fs::write(&outside, "host secret").unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("real.txt"), "real").unwrap();
        std::os::unix::fs::symlink(&outside, src.join("leak.txt")).unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert_eq!(fs::read_to_string(dst.join("real.txt")).unwrap(), "real");
        assert!(
            !dst.join("leak.txt").exists(),
            "symlink must be skipped, not copied/followed into the sandbox"
        );
    }

    #[test]
    fn reject_escaping_relpath_blocks_seed_traversal() {
        // (#897) The sandboxSeed path runs through the same escape check as
        // setupContent keys, with a seed-specific label.
        assert!(reject_escaping_relpath("sandbox", "sandboxSeed").is_ok());
        assert!(reject_escaping_relpath("sub/dir", "sandboxSeed").is_ok());
        let dotdot = reject_escaping_relpath("../../etc", "sandboxSeed").unwrap_err();
        assert!(dotdot.to_string().contains("escape the sandbox"), "got: {dotdot}");
        assert!(dotdot.to_string().contains("sandboxSeed"), "label should appear: {dotdot}");
        let abs = reject_escaping_relpath("/etc/passwd", "sandboxSeed").unwrap_err();
        assert!(abs.to_string().contains("absolute"), "got: {abs}");
    }

    #[test]
    fn read_jsonl_skips_blanks_and_malformed() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("trace.jsonl");
        fs::write(
            &p,
            r#"{"a":1}
not-valid-json
{"b":2}

{"c":3}
"#,
        )
        .unwrap();
        let parsed = read_jsonl(&p);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0]["a"], 1);
        assert_eq!(parsed[2]["c"], 3);
    }

    #[test]
    fn read_jsonl_returns_empty_on_missing() {
        let parsed = read_jsonl(Path::new("/nonexistent/path.jsonl"));
        assert!(parsed.is_empty());
    }

    #[test]
    fn classify_mode_fast() {
        let tmp = TempDir::new().unwrap();
        let mut spec = basic_spec();
        spec.expected = Some(ExpectedSpec {
            fast_cluster_seconds: Some((197, 280)),
            slow_cluster_seconds: Some((600, 950)),
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        assert_eq!(classify_mode(220_000, &loaded), Some(RunMode::Fast));
    }

    #[test]
    fn classify_mode_slow() {
        let tmp = TempDir::new().unwrap();
        let mut spec = basic_spec();
        spec.expected = Some(ExpectedSpec {
            fast_cluster_seconds: Some((197, 280)),
            slow_cluster_seconds: Some((600, 950)),
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        assert_eq!(classify_mode(700_000, &loaded), Some(RunMode::Slow));
    }

    #[test]
    fn classify_mode_unclassified() {
        let tmp = TempDir::new().unwrap();
        let mut spec = basic_spec();
        spec.expected = Some(ExpectedSpec {
            fast_cluster_seconds: Some((197, 280)),
            slow_cluster_seconds: Some((600, 950)),
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        // 400s is between fast (max 280) and slow (min 600)
        assert_eq!(classify_mode(400_000, &loaded), None);
    }

    #[test]
    fn classify_mode_no_expected_returns_none() {
        let tmp = TempDir::new().unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        assert_eq!(classify_mode(1_000_000, &loaded), None);
    }

    #[test]
    fn setup_creates_dirs_when_no_seed() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .unwrap();
        assert!(run_dir.exists());
        assert!(sandbox_dir.exists());
    }

    #[test]
    fn setup_copies_seed_when_present() {
        let tmp = TempDir::new().unwrap();
        // Create seed at base_dir/sandbox/foo.txt
        let seed_dir = tmp.path().join("sandbox");
        fs::create_dir_all(&seed_dir).unwrap();
        fs::write(seed_dir.join("foo.txt"), "seeded").unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox-out");
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .unwrap();
        assert!(sandbox_dir.join("foo.txt").exists());
        assert_eq!(
            fs::read_to_string(sandbox_dir.join("foo.txt")).unwrap(),
            "seeded"
        );
    }

    #[test]
    fn setup_bails_when_seed_dir_is_a_symlink_outside() {
        // (#897) A seed dir that passes the component check (plain `sandbox`)
        // but is itself a symlink pointing OUTSIDE the workload dir must be
        // refused by the canonicalize-and-assert-prefix guard — otherwise it
        // pulls host content into the sandbox. Red pre-fix (no canonicalize
        // assertion → the copy proceeded).
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside-seed");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "host secret").unwrap();
        let base = tmp.path().join("base");
        fs::create_dir_all(&base).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("sandbox")).unwrap();

        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox-out");
        let loaded = make_loaded(basic_spec(), base.clone());
        let err = CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .unwrap_err();
        assert!(
            err.to_string().contains("resolves outside the workload dir"),
            "expected outside-workload bail, got: {err}"
        );
        assert!(
            !sandbox_dir.join("secret.txt").exists(),
            "host content must not leak into the sandbox"
        );
    }

    /// `setupContent` writes each (path → content) pair into the sandbox
    /// dir at setup() time. Lets embedded workloads ship a complete
    /// runnable scaffold without needing an external project.
    #[test]
    fn setup_content_writes_inline_files_to_sandbox() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let mut spec = basic_spec();
        spec.setup_content
            .insert("bug.py".into(), "def buggy():\n    return None\n".into());
        spec.setup_content.insert(
            "tests/test_bug.py".into(),
            "import unittest\n\n# nested path created\n".into(),
        );
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .unwrap();
        assert_eq!(
            fs::read_to_string(sandbox_dir.join("bug.py")).unwrap(),
            "def buggy():\n    return None\n"
        );
        assert!(sandbox_dir.join("tests/test_bug.py").exists());
    }

    /// `requiresExternalSandbox` + empty sandbox + no inline setupContent
    /// → bail with operator-actionable error. This catches the new-user
    /// failure mode where the workload prompt references files the
    /// operator hasn't provided.
    #[test]
    fn setup_bails_loud_when_external_sandbox_required_but_empty() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let mut spec = basic_spec();
        spec.id = "long-agentic-style".into();
        spec.requires_external_sandbox = true;
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let err = CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("requires an external sandbox"),
            "expected actionable error; got: {msg}"
        );
        // (#490) Phase 3 — pre-Phase-3 the hint pointed at the
        // DARKMUX_SANDBOX_<W> env var. That path is removed; the
        // hint now points at the fixture-registry CLI verbs.
        assert!(
            msg.contains("darkmux lab register"),
            "expected register-verb hint; got: {msg}"
        );
        assert!(
            msg.contains("requires_fixture"),
            "expected requires_fixture mention; got: {msg}"
        );
        assert!(
            msg.contains("quick-coding"),
            "expected fallback pointer to quick-coding; got: {msg}"
        );
    }

    /// `requiresExternalSandbox` is a no-op when inline setupContent
    /// satisfies the dependency — embedded workloads can declare the
    /// flag for documentation purposes without breaking the run.
    #[test]
    fn setup_does_not_bail_when_setup_content_satisfies_external_requirement() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let mut spec = basic_spec();
        spec.requires_external_sandbox = true;
        spec.setup_content
            .insert("file.txt".into(), "content".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .expect("setupContent should satisfy requires_external_sandbox");
    }

    /// Path-traversal hardening on setupContent keys (QA BLOCK fix).
    /// An operator-installed workload manifest from a gist / friend /
    /// future install verb might include an absolute or `..`-walking
    /// key. The provider must reject before writing.
    #[test]
    fn setup_content_rejects_absolute_path() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let mut spec = basic_spec();
        spec.setup_content
            .insert("/etc/passwd".into(), "evil".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let err = CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("setupContent key") && msg.contains("/etc/passwd"),
            "expected error to name the offending key; got: {msg}"
        );
        assert!(
            !std::path::Path::new("/etc/passwd-evil-test").exists(),
            "sanity: no host file should have been written"
        );
    }

    #[test]
    fn setup_content_rejects_parent_traversal() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let mut spec = basic_spec();
        spec.setup_content
            .insert("../escape.txt".into(), "evil".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let err = CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .unwrap_err();
        assert!(
            err.to_string().contains(".."),
            "expected error to mention the traversal component; got: {err}"
        );
        assert!(
            !sandbox_dir.parent().unwrap().join("escape.txt").exists(),
            "no file should have been written outside sandbox"
        );
    }

    #[test]
    fn setup_content_accepts_safe_nested_path() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox_dir = tmp.path().join("sandbox");
        let mut spec = basic_spec();
        spec.setup_content
            .insert("a/b/c.txt".into(), "ok".into());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        CodingTaskProvider
            .setup(&loaded, &run_dir, &sandbox_dir)
            .expect("nested relative paths must be allowed");
        assert_eq!(
            fs::read_to_string(sandbox_dir.join("a/b/c.txt")).unwrap(),
            "ok"
        );
    }

    /// QA reviewer's recommendation: pin the per-runtime substitution
    /// contract. `${SANDBOX_DIR}` resolves to the operator-supplied
    /// view_path; verify substitution + the dispatch-side branching
    /// produce different paths for openclaw (host) vs internal
    /// (`/workspace`).
    #[test]
    fn expand_placeholders_with_substitutes_view_path() {
        let host = "/Users/kain/.darkmux/sandboxes/quick-coding";
        let inside_container = "/workspace";
        let prompt = "Fix the bug in ${SANDBOX_DIR}/bug.py — run python3 ${SANDBOX}/test.py";

        let host_view = expand_placeholders_with(prompt, host);
        assert!(host_view.contains(host), "openclaw path should be substituted");
        assert!(!host_view.contains("/workspace"));

        let container_view = expand_placeholders_with(prompt, inside_container);
        assert!(
            container_view.contains("/workspace/bug.py")
                && container_view.contains("/workspace/test.py")
        );
        assert!(
            !container_view.contains("/Users/"),
            "container view must NOT leak host path"
        );
    }

    #[test]
    fn inspect_handles_no_trajectory() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&run_dir).unwrap();
        // Provide minimal manifest.json
        fs::write(
            run_dir.join("manifest.json"),
            r#"{"session_id":"abc","duration_ms":12345}"#,
        )
        .unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        let report = CodingTaskProvider.inspect(&loaded, &run_dir).unwrap();
        // Manifest pre-dates `run_id`, so inspect falls back to the dir basename.
        assert_eq!(report.run_id, "run");
        assert_eq!(report.walltime_ms, 12345);
        assert_eq!(report.turns, 0);
        assert_eq!(report.compactions, 0);
    }

    /// Forward-compat: a v2-shaped manifest with `run_id` should be returned
    /// as-is, not the dir basename.
    #[test]
    fn inspect_uses_run_id_from_manifest() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("dir-name-differs");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("manifest.json"),
            r#"{"schema_version":2,"run_id":"the-canonical-id","session_id":"sess-1","duration_ms":7000}"#,
        )
        .unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        let report = CodingTaskProvider.inspect(&loaded, &run_dir).unwrap();
        assert_eq!(report.run_id, "the-canonical-id");
        assert_eq!(report.walltime_ms, 7000);
    }

    #[test]
    fn inspect_counts_turns_and_compactions() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("manifest.json"),
            r#"{"session_id":"sess","duration_ms":300000}"#,
        )
        .unwrap();
        // Three prompt.submitted events; two of them carry a unique compactionSummary.
        let trajectory = r#"{"type":"prompt.submitted","data":{"messages":[]}}
{"type":"prompt.submitted","data":{"messages":[{"role":"compactionSummary","summary":"alpha summary","tokensBefore":48000}]}}
{"type":"prompt.submitted","data":{"messages":[{"role":"compactionSummary","summary":"alpha summary","tokensBefore":48000}]}}
{"type":"prompt.submitted","data":{"messages":[{"role":"compactionSummary","summary":"beta summary text","tokensBefore":50000}]}}
"#;
        fs::write(run_dir.join("trajectory.jsonl"), trajectory).unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        let report = CodingTaskProvider.inspect(&loaded, &run_dir).unwrap();
        assert_eq!(report.turns, 4);
        assert_eq!(report.compactions, 2); // dedup by 80-char prefix
        assert_eq!(report.tokens_before, vec![48000, 50000]);
    }

    /// (#359) Internal-runtime dispatches write `metrics.json` to
    /// `<sandbox>/.darkmux-runtime/metrics.json`. Inspect must read it
    /// as the source-of-truth for turns + compactions — the
    /// trajectory's `prompt.submitted` events that the openclaw path
    /// emits are absent on the internal-runtime path, so the
    /// trajectory-derived fallback would report turns=0 by mistake.
    #[test]
    fn inspect_prefers_runtime_metrics_json_when_present() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox = tmp.path().join("sandbox");
        let runtime_dir = sandbox.join(".darkmux-runtime");
        fs::create_dir_all(&run_dir).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();
        // Manifest points at the sandbox so inspect can locate metrics.json.
        fs::write(
            run_dir.join("manifest.json"),
            format!(
                r#"{{"session_id":"sess","duration_ms":60000,"sandbox":"{}"}}"#,
                sandbox.display()
            ),
        )
        .unwrap();
        // Runtime metrics: 10 turns, 2 compactions — what the runtime
        // counted directly. No `prompt.submitted` events anywhere in
        // the trajectory because internal-runtime emits a different
        // shape; this is the pre-fix failure mode.
        fs::write(
            runtime_dir.join("metrics.json"),
            r#"{"runtime":"darkmux-runtime","version":"0.1.0","turns":10,"compactions":2}"#,
        )
        .unwrap();
        // Trajectory has zero `prompt.submitted` events on purpose —
        // representative of an internal-runtime dispatch.
        fs::write(
            run_dir.join("trajectory.jsonl"),
            r#"{"type":"model.streaming.start","seq":1,"system_chars":1000,"prompt_chars":2000}
{"type":"model.completed","seq":1}
"#,
        )
        .unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        let report = CodingTaskProvider.inspect(&loaded, &run_dir).unwrap();
        // Pre-fix: turns=0, compactions=0. Post-fix: runtime values flow through.
        assert_eq!(report.turns, 10);
        assert_eq!(report.compactions, 2);
    }

    // ─── #364: inspect prefers run_dir/metrics.json over sandbox ──

    /// When both `<run_dir>/metrics.json` (per-run preserved) AND
    /// `<sandbox>/.darkmux-runtime/metrics.json` (live source) exist,
    /// inspect prefers the run_dir copy — that's the one not subject
    /// to sandbox-overwrite by subsequent dispatches. The live source
    /// remains as a backward-compat fallback for runs predating #364
    /// (no per-run copy yet).
    #[test]
    fn inspect_prefers_run_dir_metrics_over_sandbox() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox = tmp.path().join("sandbox");
        let runtime_dir = sandbox.join(".darkmux-runtime");
        fs::create_dir_all(&run_dir).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::write(
            run_dir.join("manifest.json"),
            format!(
                r#"{{"session_id":"sess","duration_ms":60000,"sandbox":"{}"}}"#,
                sandbox.display()
            ),
        )
        .unwrap();
        // Per-run copy: 7 turns, 1 compaction. Should be picked.
        fs::write(
            run_dir.join("metrics.json"),
            r#"{"turns":7,"compactions":1}"#,
        )
        .unwrap();
        // Sandbox live source: 99 turns, 99 compactions. Should be
        // IGNORED in favor of the per-run copy.
        fs::write(
            runtime_dir.join("metrics.json"),
            r#"{"turns":99,"compactions":99}"#,
        )
        .unwrap();
        fs::write(run_dir.join("trajectory.jsonl"), "").unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        let report = CodingTaskProvider.inspect(&loaded, &run_dir).unwrap();
        // Per-run copy wins.
        assert_eq!(report.turns, 7);
        assert_eq!(report.compactions, 1);
    }

    /// (#359 QA follow-up) When `.darkmux-runtime/metrics.json` exists
    /// but is malformed, inspect must fall back to trajectory-derived
    /// counts rather than surfacing a parse error. The
    /// `read_internal_runtime_metrics` helper uses `.ok()` short-
    /// circuits at every step; this test locks that contract so a
    /// future refactor doesn't accidentally start propagating the
    /// error.
    #[test]
    fn inspect_falls_back_when_runtime_metrics_is_malformed() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox = tmp.path().join("sandbox");
        let runtime_dir = sandbox.join(".darkmux-runtime");
        fs::create_dir_all(&run_dir).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::write(
            run_dir.join("manifest.json"),
            format!(
                r#"{{"session_id":"sess","duration_ms":60000,"sandbox":"{}"}}"#,
                sandbox.display()
            ),
        )
        .unwrap();
        // Garbage instead of valid JSON.
        fs::write(runtime_dir.join("metrics.json"), "not valid json {{ <}}").unwrap();
        let trajectory = r#"{"type":"prompt.submitted","data":{"messages":[]}}
{"type":"prompt.submitted","data":{"messages":[{"role":"compactionSummary","summary":"a summary","tokensBefore":42000}]}}
"#;
        fs::write(run_dir.join("trajectory.jsonl"), trajectory).unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        let report = CodingTaskProvider
            .inspect(&loaded, &run_dir)
            .expect("malformed runtime metrics must not abort inspect");
        // Trajectory-derived counts kick in; matches the existing
        // `inspect_counts_turns_and_compactions` shape.
        assert_eq!(report.turns, 2);
        assert_eq!(report.compactions, 1);
    }

    /// Backward-compat: when no runtime metrics.json exists (the
    /// openclaw shell-out path), inspect falls back to deriving
    /// turns + compactions from the trajectory's `prompt.submitted`
    /// events. The existing
    /// `inspect_counts_turns_and_compactions` test covers the happy
    /// path; this one specifically asserts the fallback fires when
    /// `manifest.sandbox` points at a directory with no
    /// `.darkmux-runtime/metrics.json`.
    #[test]
    fn inspect_falls_back_to_trajectory_when_no_runtime_metrics() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        let sandbox = tmp.path().join("sandbox");
        fs::create_dir_all(&run_dir).unwrap();
        fs::create_dir_all(&sandbox).unwrap();
        // sandbox exists but no .darkmux-runtime subdir — fallback path.
        fs::write(
            run_dir.join("manifest.json"),
            format!(
                r#"{{"session_id":"sess","duration_ms":60000,"sandbox":"{}"}}"#,
                sandbox.display()
            ),
        )
        .unwrap();
        let trajectory = r#"{"type":"prompt.submitted","data":{"messages":[]}}
{"type":"prompt.submitted","data":{"messages":[{"role":"compactionSummary","summary":"a summary","tokensBefore":42000}]}}
"#;
        fs::write(run_dir.join("trajectory.jsonl"), trajectory).unwrap();
        let loaded = make_loaded(basic_spec(), tmp.path().to_path_buf());
        let report = CodingTaskProvider.inspect(&loaded, &run_dir).unwrap();
        assert_eq!(report.turns, 2, "trajectory-derived fallback");
        assert_eq!(report.compactions, 1, "trajectory-derived fallback");
    }

    #[test]
    fn inspect_classifies_fast_when_expected_set() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("manifest.json"),
            r#"{"session_id":"x","duration_ms":220000}"#,
        )
        .unwrap();
        let mut spec = basic_spec();
        spec.expected = Some(ExpectedSpec {
            fast_cluster_seconds: Some((197, 280)),
            slow_cluster_seconds: Some((600, 950)),
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let report = CodingTaskProvider.inspect(&loaded, &run_dir).unwrap();
        assert_eq!(report.mode, Some(RunMode::Fast));
    }

    // Note: `pathdiff_relative` and `pathdiff_same` tests removed in
    // #359 along with the `pathdiff` mod they covered.

    #[test]
    fn run_verify_command_returns_none_when_no_command() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&run_dir).unwrap();
        let mut spec = basic_spec();
        spec.verify = Some(VerifySpec::default());
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let result = run_verify_command(&loaded, &run_dir, tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn run_verify_command_executes_command() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&run_dir).unwrap();
        let mut spec = basic_spec();
        spec.verify = Some(VerifySpec {
            command: Some("true".into()),
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let result = run_verify_command(&loaded, &run_dir, tmp.path())
            .unwrap()
            .unwrap();
        assert!(result.passed);
    }

    #[test]
    fn run_verify_command_reports_failure() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&run_dir).unwrap();
        let mut spec = basic_spec();
        spec.verify = Some(VerifySpec {
            command: Some("false".into()),
            ..Default::default()
        });
        let loaded = make_loaded(spec, tmp.path().to_path_buf());
        let result = run_verify_command(&loaded, &run_dir, tmp.path())
            .unwrap()
            .unwrap();
        assert!(!result.passed);
        assert!(result.details.contains("exit"));
    }

    // ─── #420: verify-claim disagreement detection ──────────────────────

    fn verify_passed() -> crate::workloads::types::VerifyOutcome {
        crate::workloads::types::VerifyOutcome {
            passed: true,
            details: "verify command exited 0".into(),
        }
    }

    fn verify_failed() -> crate::workloads::types::VerifyOutcome {
        crate::workloads::types::VerifyOutcome {
            passed: false,
            details: "exit 1".into(),
        }
    }

    #[test]
    fn mismatch_detected_when_agent_claims_all_tests_pass_but_verify_failed() {
        let final_msg = "I made the edits. All tests pass. Ready for review.";
        let mm = detect_claim_verify_mismatch(final_msg, Some(&verify_failed()));
        assert!(mm.is_some(), "expected mismatch on positive claim + verify fail");
        let mm = mm.unwrap();
        // (#869) The excerpt is now built from the lowercased copy (forensic).
        assert!(mm.claim_excerpt.contains("all tests pass"));
        assert_eq!(mm.verify_details, "exit 1");
    }

    #[test]
    fn mismatch_excerpt_handles_non_ascii_without_panic() {
        // (#869) Regression: a multibyte window around the claim must not panic
        // the excerpt byte-slice. `İ`/accents/`Ω`/emoji change byte length under
        // to_lowercase(), so indices from `lowered` would split a codepoint in
        // `final_assistant`. The prefix is >80 bytes of multibyte text so the
        // ±80 window lands inside it (pre-#869 this panicked).
        let pad = "café señor İstanbul Ωμέγα 🎉 ".repeat(5);
        let final_msg = format!("{pad}all tests pass{pad}");
        let mm = detect_claim_verify_mismatch(&final_msg, Some(&verify_failed()));
        assert!(mm.is_some(), "should detect the claim despite non-ASCII context");
        assert!(
            mm.unwrap().claim_excerpt.contains("all tests pass"),
            "excerpt should still contain the matched claim"
        );
    }

    #[test]
    fn mismatch_detected_for_successfully_completed_phrasing() {
        let final_msg = "Implementation done. Successfully completed all 3 files.";
        let mm = detect_claim_verify_mismatch(final_msg, Some(&verify_failed()));
        assert!(mm.is_some());
    }

    #[test]
    fn mismatch_detected_for_no_failures_phrasing() {
        let final_msg = "Reviewed the changes. No failures observed.";
        let mm = detect_claim_verify_mismatch(final_msg, Some(&verify_failed()));
        assert!(mm.is_some());
    }

    #[test]
    fn no_mismatch_when_agent_acknowledges_still_failing() {
        // Agent was honest about the partial state; no claim of completion.
        let final_msg = "I fixed 2 of 3 tests. The third is still failing — \
                         I need help understanding the mock setup. All tests pass once that's resolved.";
        let mm = detect_claim_verify_mismatch(final_msg, Some(&verify_failed()));
        assert!(mm.is_none(), "honesty marker 'still failing' must override positive claim");
    }

    #[test]
    fn no_mismatch_when_agent_says_partial() {
        let final_msg = "Partial completion. Ran out of context budget. \
                         All tests pass for the files I did edit.";
        let mm = detect_claim_verify_mismatch(final_msg, Some(&verify_failed()));
        assert!(mm.is_none(), "honesty marker 'partial' must override");
    }

    #[test]
    fn no_mismatch_when_agent_acknowledges_remaining_failure() {
        let final_msg = "Edits applied. All tests pass on services/foo. \
                         1 remaining failure in services/bar — pepper config issue.";
        let mm = detect_claim_verify_mismatch(final_msg, Some(&verify_failed()));
        assert!(mm.is_none(), "honesty marker 'remaining failure' must override");
    }

    #[test]
    fn no_mismatch_when_no_completion_claim_in_message() {
        // Agent provided a substantive summary but didn't claim
        // completion. Detector returns None — we can't tell either way.
        let final_msg = "I edited 3 files. The diff is ready for your review.";
        let mm = detect_claim_verify_mismatch(final_msg, Some(&verify_failed()));
        assert!(mm.is_none(), "no positive claim → no detection signal");
    }

    #[test]
    fn no_mismatch_when_verify_passed() {
        let final_msg = "All tests pass. Ready for review.";
        let mm = detect_claim_verify_mismatch(final_msg, Some(&verify_passed()));
        assert!(mm.is_none(), "verify passing means claim is correct, not a mismatch");
    }

    #[test]
    fn no_mismatch_when_no_verify_outcome_at_all() {
        // Workload has no verify command; we can't compare.
        let final_msg = "All tests pass. Ready for review.";
        let mm = detect_claim_verify_mismatch(final_msg, None);
        assert!(mm.is_none(), "no verify outcome → no detection signal");
    }

    #[test]
    fn no_mismatch_on_empty_final_assistant() {
        // Silent bail (per the V4 N=5 Run 2 pattern). No claim to detect.
        let mm = detect_claim_verify_mismatch("", Some(&verify_failed()));
        assert!(mm.is_none());
    }

    #[test]
    fn detection_is_case_insensitive() {
        let final_msg = "ALL TESTS PASS now.";
        let mm = detect_claim_verify_mismatch(final_msg, Some(&verify_failed()));
        assert!(mm.is_some(), "match should be case-insensitive");
    }

    #[test]
    fn claim_excerpt_centers_on_matched_phrase() {
        let final_msg = "I refactored the auth module to use the new pepper config. \
                         All tests pass. The diff is ~200 lines.";
        let mm = detect_claim_verify_mismatch(final_msg, Some(&verify_failed())).unwrap();
        // (#869) The excerpt is built from the lowercased copy now (forensic).
        assert!(mm.claim_excerpt.contains("all tests pass"));
        // Excerpt should include surrounding context, not just the bare match.
        assert!(mm.claim_excerpt.len() > "all tests pass".len());
    }

    #[test]
    fn augment_qa_reply_adds_claim_verify_mismatch_field() {
        // Integration: write a representative qa-reply.json, run the
        // augmentor, parse back, assert the new field landed and the
        // existing fields are preserved.
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path();
        let initial = serde_json::json!({
            "final_assistant": "All tests pass.",
            "result": "stop",
            "metrics": { "turns": 5, "wall_ms": 1000 }
        });
        fs::write(
            run_dir.join("qa-reply.json"),
            serde_json::to_string(&initial).unwrap(),
        )
        .unwrap();

        let mm = ClaimVerifyMismatch {
            claim_excerpt: "All tests pass.".into(),
            verify_details: "exit 1".into(),
        };
        augment_qa_reply_with_mismatch(run_dir, &mm).unwrap();

        let raw = fs::read_to_string(run_dir.join("qa-reply.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // New field landed
        let cvm = parsed.get("claim_verify_mismatch").expect("field must be present");
        assert_eq!(cvm["claim_excerpt"], "All tests pass.");
        assert_eq!(cvm["verify_details"], "exit 1");
        // Existing fields preserved
        assert_eq!(parsed["result"], "stop");
        assert_eq!(parsed["metrics"]["turns"], 5);
        assert_eq!(parsed["final_assistant"], "All tests pass.");
    }

    #[test]
    fn augment_qa_reply_silently_skips_when_file_missing() {
        // Best-effort: missing qa-reply.json is logged but doesn't
        // fail the dispatch.
        let tmp = TempDir::new().unwrap();
        let mm = ClaimVerifyMismatch {
            claim_excerpt: "x".into(),
            verify_details: "y".into(),
        };
        let result = augment_qa_reply_with_mismatch(tmp.path(), &mm);
        assert!(result.is_ok(), "missing qa-reply.json must not bubble up an error");
    }

    #[test]
    fn augment_qa_reply_silently_skips_when_file_malformed() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path();
        fs::write(run_dir.join("qa-reply.json"), "not valid json {").unwrap();
        let mm = ClaimVerifyMismatch {
            claim_excerpt: "x".into(),
            verify_details: "y".into(),
        };
        let result = augment_qa_reply_with_mismatch(run_dir, &mm);
        assert!(result.is_ok(), "malformed qa-reply.json must not bubble up an error");
        // Original content preserved (parse failure path).
        let raw = fs::read_to_string(run_dir.join("qa-reply.json")).unwrap();
        assert_eq!(raw, "not valid json {");
    }

    // ─── #421: workspace_delta augmentation ──────────────────────────────

    #[test]
    fn augment_qa_reply_adds_workspace_delta_field() {
        use crate::providers::workspace_delta::WorkspaceDelta;
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path();
        let initial = serde_json::json!({
            "final_assistant": "done",
            "result": "stop",
            "metrics": { "turns": 5 }
        });
        fs::write(
            run_dir.join("qa-reply.json"),
            serde_json::to_string(&initial).unwrap(),
        )
        .unwrap();

        let delta = WorkspaceDelta {
            added: vec![PathBuf::from("tests/new.test.ts")],
            modified: vec![PathBuf::from("src/services/foo.ts")],
            removed: vec![],
            total_bytes_changed: 1240,
        };
        augment_qa_reply_with_workspace_delta(run_dir, &delta).unwrap();

        let raw = fs::read_to_string(run_dir.join("qa-reply.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let wd = parsed.get("workspace_delta").expect("field must be present");
        assert_eq!(wd["added"], serde_json::json!(["tests/new.test.ts"]));
        assert_eq!(wd["modified"], serde_json::json!(["src/services/foo.ts"]));
        assert_eq!(wd["removed"], serde_json::json!([] as [String; 0]));
        assert_eq!(wd["total_bytes_changed"], 1240);
        // Existing fields preserved
        assert_eq!(parsed["result"], "stop");
        assert_eq!(parsed["metrics"]["turns"], 5);
    }

    #[test]
    fn augment_qa_reply_workspace_delta_silently_skips_when_file_missing() {
        use crate::providers::workspace_delta::WorkspaceDelta;
        let tmp = TempDir::new().unwrap();
        let delta = WorkspaceDelta::default();
        let result = augment_qa_reply_with_workspace_delta(tmp.path(), &delta);
        assert!(result.is_ok());
    }

    #[test]
    fn augment_qa_reply_workspace_delta_silently_skips_when_file_malformed() {
        use crate::providers::workspace_delta::WorkspaceDelta;
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path();
        fs::write(run_dir.join("qa-reply.json"), "garbage {").unwrap();
        let delta = WorkspaceDelta::default();
        let result = augment_qa_reply_with_workspace_delta(run_dir, &delta);
        assert!(result.is_ok());
        // Preserved on parse failure
        assert_eq!(fs::read_to_string(run_dir.join("qa-reply.json")).unwrap(), "garbage {");
    }

    #[test]
    fn augment_qa_reply_workspace_delta_composes_with_claim_mismatch() {
        // Both #420 augmentation and #421 augmentation should be able
        // to compose on the same qa-reply.json. Calling them in
        // sequence should leave BOTH fields present.
        use crate::providers::workspace_delta::WorkspaceDelta;
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path();
        fs::write(
            run_dir.join("qa-reply.json"),
            serde_json::to_string(&serde_json::json!({
                "final_assistant": "x",
                "result": "stop"
            })).unwrap(),
        )
        .unwrap();

        let mm = ClaimVerifyMismatch {
            claim_excerpt: "All tests pass.".into(),
            verify_details: "exit 1".into(),
        };
        augment_qa_reply_with_mismatch(run_dir, &mm).unwrap();

        let delta = WorkspaceDelta {
            added: vec![],
            modified: vec![PathBuf::from("src/foo.rs")],
            removed: vec![],
            total_bytes_changed: 100,
        };
        augment_qa_reply_with_workspace_delta(run_dir, &delta).unwrap();

        let raw = fs::read_to_string(run_dir.join("qa-reply.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(parsed.get("claim_verify_mismatch").is_some(), "claim_verify_mismatch must survive");
        assert!(parsed.get("workspace_delta").is_some(), "workspace_delta must land");
        assert_eq!(parsed["result"], "stop");
    }
}
