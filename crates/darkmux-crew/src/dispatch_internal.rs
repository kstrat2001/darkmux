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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Docker image tag for the internal runtime. Built locally from
/// `runtime/Dockerfile`. Will become configurable when production
/// hardening lands.
const RUNTIME_IMAGE: &str = "darkmux-runtime:latest";

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
fn apply_volume_mounts(cmd: &mut Command, workspace: &Path, host_out: &Path) {
    cmd.arg("-v")
        .arg(format!("{}:/workspace", workspace.display()))
        .arg("-v")
        .arg(format!("{}:/darkmux-out", host_out.display()));
}

/// (#368) Translate the host-side `CompactionDispatchArgs` into
/// runtime CLI flags. Each `Some(v)` becomes a `--flag v` pair on
/// the docker run command; `None` is omitted so the runtime falls
/// back to its hardcoded default for that knob. Extracted from the
/// docker-spawn site so the translation rule is unit-testable
/// without spawning a container.
fn apply_compaction_flags(
    cmd: &mut Command,
    compaction: &crate::dispatch::CompactionDispatchArgs,
) {
    if let Some(n) = compaction.threshold_tokens {
        cmd.arg("--compact-threshold-tokens").arg(n.to_string());
    }
    if let Some(model) = &compaction.compactor_model {
        cmd.arg("--compactor-model").arg(model);
    }
    if let Some(share) = compaction.threshold_ratio {
        cmd.arg("--compact-threshold-ratio").arg(share.to_string());
    }
    if let Some(window) = compaction.context_window {
        cmd.arg("--context-window").arg(window.to_string());
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
        cmd.arg("--compact-strategy").arg(kebab);
    }
    // (#377) Escalation bound → `--bail-after-compactions N`.
    // Runtime exits with EscalationTriggered when this many
    // compactions have occurred. None ⇒ flag omitted ⇒ runtime is
    // unbounded (back-compat with pre-#377 behavior).
    if let Some(n) = compaction.bail_after_compactions {
        cmd.arg("--bail-after-compactions").arg(n.to_string());
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
        cmd.arg("--compactor-custom-instructions").arg(text);
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
    if let Ok(raw) = std::env::var("DARKMUX_RUNTIME_MAX_TURNS") {
        match raw.parse::<u32>() {
            Ok(n) => {
                cmd.arg("--max-turns").arg(n.to_string());
            }
            Err(_) => {
                eprintln!(
                    "darkmux crew dispatch: DARKMUX_RUNTIME_MAX_TURNS=`{raw}` is not a \
                     positive integer; ignoring (runtime defaults to unlimited turns). (#457)"
                );
            }
        }
    }
    if let Ok(raw) = std::env::var("DARKMUX_RUNTIME_MAX_TOKENS") {
        match raw.parse::<u32>() {
            Ok(n) => {
                cmd.arg("--max-tokens").arg(n.to_string());
            }
            Err(_) => {
                eprintln!(
                    "darkmux crew dispatch: DARKMUX_RUNTIME_MAX_TOKENS=`{raw}` is not a \
                     positive integer; ignoring (runtime defaults to unlimited cumulative \
                     completion tokens). (#457)"
                );
            }
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
/// **600s** is the same value the prior absolute deadline used.
/// Under inactivity-reset semantics it bounds the time between
/// progress signals rather than total dispatch wall-clock —
/// dispatches making compactions every ~5-10 min stay alive
/// indefinitely up to the runtime's other bounds (per-call token
/// cap, cumulative-tokens cap, MAX_TURNS).
///
/// Override per dispatch via `DARKMUX_INACTIVITY_TIMEOUT_SECONDS`.
///
/// (#457) Renamed from `DEFAULT_DISPATCH_DEADLINE_SECS` /
/// `DARKMUX_RUNTIME_DEADLINE_SECONDS`. The prior absolute-deadline
/// semantics killed dispatches making observable progress (Beat 53b:
/// 88 passing tests, killed at 600s with the model still iterating).
/// Progress-signal-based limits trust empirical evidence; absolute
/// caps embed a guess about how long good work should take.
const DEFAULT_INACTIVITY_TIMEOUT_SECS: u64 = 600;

fn inactivity_timeout_seconds() -> u64 {
    std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_INACTIVITY_TIMEOUT_SECS)
}

/// LMStudio /v1/models URL used to probe the currently-loaded model
/// when no explicit model is provided. Currently the internal runtime
/// uses "whatever's loaded"; future iteration will resolve via the
/// role pin table.
const LMSTUDIO_MODELS_URL: &str = "http://localhost:1234/v1/models";

pub fn dispatch(opts: DispatchOpts) -> Result<DispatchResult> {
    eprintln!(
        "darkmux crew dispatch: runtime=internal — image: {RUNTIME_IMAGE}"
    );

    // Pre-flight: Docker reachable + runtime image present. The
    // internal runtime is the default as of the runtime-default flip;
    // these are the prereqs a new user might not have set up yet. Bail
    // loud + operator-actionable BEFORE we run the role-load / model-
    // probe / workspace-setup work below.
    if !opts.skip_preflight {
        check_docker_preflight()?;
    }

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
    //    to the Primary-role model. If no profile is configured (or
    //    has no Primary), falls back to `probe_loaded_model()` with a
    //    deprecation warning — back-compat for operators on the
    //    pre-refactor-1b config shape; the warning surfaces the gap
    //    so they migrate.
    //
    //    NOT the long-form probe-then-pin path documented in #408 —
    //    that's phase 2+ scope when the recommendation registry
    //    activates per-hardware tuple selection.
    let model = resolve_dispatch_model_internal(role).context(
        "model selection failed. Ensure `~/.darkmux/profiles.json` has \
         a profile with a model `role: \"primary\"`, or load a model in \
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

    // 5. Emit dispatch.start flow record with runtime metadata in payload
    //    (#204). Pairs with dispatch.complete below via session_id, same
    //    as the openclaw path does.
    let dispatch_start_payload = serde_json::json!({
        "runtime": "internal",
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
        Some(dispatch_start_payload),
    ));
    let dispatch_start_instant = std::time::Instant::now();

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

    let mut cmd = Command::new("docker");
    cmd.arg("run")
        .arg("--rm")
        .arg("--name")
        .arg(&container_name);
    // `/workspace` (the agent's tree) + `/darkmux-out` (the runtime's
    // out-of-band bookkeeping). `/darkmux-out` MUST match
    // `runtime::trajectory::RUNTIME_OUT_BASE` — see `apply_volume_mounts`.
    apply_volume_mounts(&mut cmd, &workspace, &host_out);
    cmd.arg(RUNTIME_IMAGE)
        .arg("run")
        .arg("--model")
        .arg(&model)
        .arg("--system")
        .arg(&system_prompt)
        .arg("--prompt")
        .arg(&opts.message);
    if opts.json {
        // Plumb the operator's `--json` request through to the
        // container CLI so downstream parsers (qa-review skill, lab
        // adapter, ad-hoc `jq` users) get a structured envelope on
        // stdout instead of the human-readable separator format. The
        // runtime emits status lines to stderr in JSON mode so stdout
        // stays clean. See `runtime/src/main.rs::build_json_envelope`
        // for the schema contract.
        cmd.arg("--json");
    }
    if let Some(allowed) = allowed_tools.as_ref() {
        let csv = allowed.join(",");
        eprintln!(
            "darkmux crew dispatch: tool_palette filtered to [{}] (role={})",
            csv, opts.role_id
        );
        cmd.arg("--allowed-tools").arg(csv);
    }
    // (#368) Compaction config passthrough to the runtime CLI. Each
    // flag is optional: when the operator's profile didn't set a
    // value, no flag is passed and the runtime uses its hardcoded
    // default. End-state: operator tunes via profile JSON → these
    // four flags flow into the container → no env vars touch the
    // surface anywhere.
    // (#377) Per-role escalation-bound overlay: role manifest's
    // `bail_after_compactions` wins over profile fallback when set.
    // Lookup chain: role > profile > unset. The role was already
    // loaded at the top of this function.
    let mut compaction = opts.compaction.clone();
    compaction.apply_role_override(role);
    // (#590) Overlay the machine-level utility model (`internal.utility`) as
    // the compactor, unless already pinned. The util model is darkmux's
    // standing support model for this machine — one global model for
    // compaction, decoupled from the worker profile. Best-effort: no binding
    // ⇒ untouched ⇒ runtime keeps its built-in default compactor.
    let utility_model = resolve_utility_model_internal();
    compaction.apply_utility_model(utility_model.as_deref());
    // (#590) Pre-flight loaded-check: compaction summons the util model
    // mid-dispatch, so if it's registered but not resident the compactor call
    // fails. Warn loudly here (the doctor check is the at-rest sibling).
    // Best-effort — skipped when lms is unreachable.
    if let Some(util_id) = compaction.compactor_model.as_deref() {
        if let Ok(loaded) = darkmux_profiles::lms::list_loaded() {
            if let Some(warning) = utility_preflight_warning(util_id, &loaded) {
                eprintln!("{warning}");
            }
        }
    }
    apply_compaction_flags(&mut cmd, &compaction);

    // (#457 Changes 2+3) Operator-opt-in per-dispatch caps on turn
    // count + cumulative completion tokens. Read from env vars on
    // host side; pass via --max-turns / --max-tokens CLI flags to
    // the runtime container. Both default unlimited (omitted flag
    // → runtime's `Option<u32>` stays None → no cap applied).
    apply_runtime_limit_flags(&mut cmd);

    // (#457 Step 2) Per-role feedback-template overrides. If the
    // role manifest declares `feedback_templates`, serialize the
    // map to JSON and pass via `--feedback-templates-json`. The
    // runtime parses and overrides the FeedbackInjector's defaults
    // for any signal-kind key present. Absent field ⇒ no flag ⇒
    // runtime uses its hardcoded defaults across all signals.
    if let Some(templates) = role.feedback_templates.as_ref() {
        if !templates.is_empty() {
            match serde_json::to_string(templates) {
                Ok(json) => {
                    cmd.arg("--feedback-templates-json").arg(json);
                }
                Err(e) => {
                    eprintln!(
                        "darkmux crew dispatch: failed to serialize role `{}` \
                         feedback_templates: {e}. Runtime will use defaults. \
                         (#457 Step 2)",
                        opts.role_id
                    );
                }
            }
        }
    }

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
        let inactivity_deadline = Arc::clone(&inactivity_deadline);
        thread::spawn(move || {
            run_tailer(
                out_dir,
                session_id,
                role_id,
                model,
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
                let deadline = *inactivity_deadline.lock().unwrap();
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

    let output = child
        .wait_with_output()
        .context("waiting for darkmux-runtime container")?;

    // Tell the watchdog we're done so it doesn't fire spuriously after
    // a natural exit. Best-effort join (it's a kill-only thread —
    // panic-resilience isn't load-bearing).
    watchdog_done.store(true, Ordering::SeqCst);
    let _ = watchdog_handle.join();

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
        Some(dispatch_complete_payload),
    ));

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

/// How often the live tailer polls `trajectory.jsonl` while the container
/// is alive. 250ms matches the daemon's `tail_lines` poll cadence in
/// `serve.rs` — short enough for sub-second responsiveness, long enough
/// to keep CPU+IO cost negligible for an idle dispatch.
const TAILER_POLL_INTERVAL: Duration = Duration::from_millis(250);

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

/// Run the live trajectory tailer to completion. Polls until `stop_flag`
/// is set, then does one final flush pass to drain any straggler lines
/// the container wrote between the last poll tick and exit. Returns the
/// accumulated event-count summary; never errors (observability path).
///
/// (#457) `inactivity_deadline` is shared with the watchdog thread;
/// the tailer writes a new deadline to it each time a `compaction`
/// trajectory event lands. `inactivity_secs` is the timeout window
/// used to compute the new deadline (`now + inactivity_secs`).
fn run_tailer(
    // Host path mounted into the container at `/darkmux-out` — where the
    // runtime writes its `.darkmux-runtime/trajectory.jsonl`. SEPARATE
    // from the workspace so the tailer reads the runtime's own
    // bookkeeping, not the tree the agent is editing.
    out_dir: PathBuf,
    session_id: String,
    role_id: String,
    model: String,
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
    );

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
            last_heartbeat_at: None,
            summary: TrajectorySummary::default(),
            inactivity_deadline: Some(inactivity_deadline),
            inactivity_secs,
        }
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
                    "finish_reason": event.get("finish_reason"),
                    "tool_calls_count": event.get("tool_calls").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
                    "usage": event.get("usage"),
                });
                self.emit("dispatch.turn", darkmux_flow::Level::Info, payload);
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
                        if let Ok(mut guard) = deadline.lock() {
                            *guard = new_deadline;
                        }
                    }
                }
                let payload = serde_json::json!({
                    "tool_seq": event.get("tool_seq"),
                    "tool_name": event.get("tool_name"),
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
                    if let Ok(mut guard) = deadline.lock() {
                        *guard = new_deadline;
                    }
                }
                let payload = serde_json::json!({
                    "generation": event.get("generation"),
                    "before_messages": event.get("before_messages"),
                    "after_messages": event.get("after_messages"),
                    "summary_chars": event.get("summary_chars"),
                });
                self.emit("dispatch.compaction", darkmux_flow::Level::Info, payload);
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
            Some(payload),
        ));
    }
}

/// Cap a JSON string value at `MAX_REASONING_TEXT_BYTES`, appending a
/// human-readable marker so downstream consumers know it was truncated.
/// Truncates at a UTF-8 char boundary to avoid invalid encoding.
/// Non-string values pass through unchanged. (#231 / S6)
fn cap_reasoning_text(value: Option<&serde_json::Value>) -> serde_json::Value {
    let Some(v) = value else {
        return serde_json::Value::Null;
    };
    let Some(s) = v.as_str() else {
        return v.clone();
    };
    if s.len() <= MAX_REASONING_TEXT_BYTES {
        return v.clone();
    }
    // Truncate at a UTF-8 char boundary so the resulting string is valid.
    let mut cut = MAX_REASONING_TEXT_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let original_bytes = s.len();
    let original_chars = s.chars().count();
    serde_json::Value::String(format!(
        "{}… [truncated; original {original_chars} chars / {original_bytes} bytes]",
        &s[..cut]
    ))
}

/// Shell out to curl to fetch `/v1/models` from the host's LMStudio and
/// Verify Docker is reachable + the runtime image exists. Called by
/// `dispatch()` BEFORE the role-load / model-probe / workspace setup
/// so a new user without Docker (or with Docker but no runtime image)
/// gets a clean, operator-actionable bail message instead of an
/// opaque `Command::new("docker")` "No such file or directory" or
/// a runtime-time "Unable to find image" failure mid-dispatch.
fn check_docker_preflight() -> Result<()> {
    // Step 1: docker binary exists + daemon is reachable.
    let docker_check = Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output();
    match docker_check {
        Ok(out) if out.status.success() => {} // Docker daemon up
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!(
                "darkmux's default runtime (`--runtime internal`) requires Docker, \
                 but `docker version` failed:\n  {}\n\
                 Options:\n  \
                 - Start Docker Desktop, OR\n  \
                 - Re-run with `--runtime openclaw` if you have openclaw installed",
                stderr.trim()
            );
        }
        Err(_) => {
            bail!(
                "darkmux's default runtime (`--runtime internal`) requires Docker, \
                 but the `docker` binary isn't on PATH.\n\
                 Options:\n  \
                 - Install Docker Desktop (https://www.docker.com/products/docker-desktop), OR\n  \
                 - Re-run with `--runtime openclaw` if you have openclaw installed"
            );
        }
    }

    // Step 2: runtime image exists locally. `docker images -q <tag>`
    // exits 0 even when the image is missing; the load-bearing signal
    // is empty stdout (no image id printed). Daemon-unreachable cases
    // were already caught in Step 1.
    let image_check = Command::new("docker")
        .args(["images", "-q", RUNTIME_IMAGE])
        .output()
        .context("running `docker images` to check for runtime image")?;
    if image_check.stdout.is_empty() {
        bail!(
            "darkmux runtime image `{RUNTIME_IMAGE}` not found locally.\n\
             Build it once from the darkmux repo root:\n  \
             docker build -t {RUNTIME_IMAGE} runtime/\n\
             (Or use `--runtime openclaw` if you have openclaw installed.)"
        );
    }
    Ok(())
}

/// (#450, E14 refactor 1b) Resolve the model id this internal-runtime
/// dispatch should target for the given role.
///
/// Selection chain:
/// 1. Load the profile registry. Get the active profile name from
///    `registry.default_profile` (phase-1 simplification — operators
///    set the default; if they want a different profile, they swap
///    the default). Phase 2+ adds `--profile <name>` plumbing through
///    `DispatchOpts` for per-dispatch override.
/// 2. Look up the profile + call `select_model(role, profile, skill_lookup)`,
///    which capability-scores the role against the profile's models (phase 2),
///    falling back to the Primary-role model when no vectors are populated.
/// 3. On any failure (no registry, no default, no profile, no Primary),
///    log a deprecation warning + fall back to `probe_loaded_model()`.
///    Back-compat for pre-refactor-1b configurations; the warning
///    points the operator at the migration.
///
/// The fallback is intentional but loud. Per memory note
/// `feedback_model_unload_load_authority`, silent reliance on "whatever
/// LMStudio happens to have loaded" is the contaminating-dispatch
/// anti-pattern. The deprecation warning makes the misconfiguration
/// operator-visible while keeping pre-refactor-1b setups working.
fn resolve_dispatch_model_internal(role: &crate::types::Role) -> Result<String> {
    use crate::select::select_model;
    use darkmux_profiles::profiles::{get_profile, load_registry};

    let loaded = match load_registry(None) {
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

    let active_name = match loaded.registry.default_profile.as_deref() {
        Some(name) => name,
        None => {
            eprintln!(
                "darkmux crew dispatch: profile registry has no default_profile set; \
                 falling back to probe_loaded_model() — deprecated, set \
                 default_profile in ~/.darkmux/profiles.json. (#450 refactor 1b)"
            );
            return probe_loaded_model();
        }
    };

    let profile = match get_profile(&loaded.registry, active_name) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "darkmux crew dispatch: default_profile `{active_name}` not in \
                 registry ({e}); falling back to probe_loaded_model() — \
                 deprecated. (#450 refactor 1b)"
            );
            return probe_loaded_model();
        }
    };

    // (#590 phase 2) Build a skill lookup so select_model can compose the
    // role's requested capability vector (role → skills → CapabilityProfile).
    // Skills unavailable ⇒ empty lookup ⇒ select_model takes its Primary
    // fallback (safe + behavior-preserving).
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
            // balanced's Primary while LMStudio is loaded with fast's
            // models. The dispatch would then fail at the LMStudio call
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
                         Primary is `{id}`, but LMStudio has loaded [{loaded}]. \
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
                "darkmux crew dispatch: model selected via profile `{active_name}` Primary"
            );
            Ok(id)
        }
        Err(e) => {
            eprintln!(
                "darkmux crew dispatch: select_model error ({e}); falling back \
                 to probe_loaded_model() — deprecated. Add a Primary-role \
                 model to profile `{active_name}` to migrate. (#450 refactor 1b)"
            );
            // TODO(#450 phase-1c): the selected-vs-loaded MISMATCH case
            // now honors `DARKMUX_STRICT_SELECTION` (see the Ok branch
            // above). This Err branch — no Primary configured at all —
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
fn resolve_utility_model_internal() -> Option<String> {
    darkmux_profiles::profiles::load_registry(None)
        .ok()
        .and_then(|l| l.registry.utility_model_id().map(str::to_string))
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
    std::env::var("DARKMUX_STRICT_SELECTION")
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
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

// `first_user_symlink_in` and `is_macos_firmlink` moved to
// `darkmux_types::workdir` as part of Wave-E.2 (#255). Workers + both runtime
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

    // ─── #368: compaction-flag passthrough to runtime CLI ────────────

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    // ─── out-of-band bookkeeping: volume mounts ──────────────────────

    #[test]
    fn apply_volume_mounts_emits_workspace_and_out_dir() {
        // The runtime's OWN bookkeeping goes to `/darkmux-out`, SEPARATE
        // from the agent's `/workspace`. This literal MUST stay in sync
        // with `runtime::trajectory::RUNTIME_OUT_BASE` (the two crates
        // can't share the const — the runtime is built into the image).
        let mut cmd = Command::new("docker");
        apply_volume_mounts(
            &mut cmd,
            Path::new("/host/workspace"),
            Path::new("/host/out"),
        );
        let args = args_of(&cmd);
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
        let mut cmd = Command::new("docker");
        let compaction = crate::dispatch::CompactionDispatchArgs::default();
        apply_compaction_flags(&mut cmd, &compaction);
        let args = args_of(&cmd);
        assert!(
            !args.iter().any(|a| a.starts_with("--compact") || a == "--context-window"),
            "default config should emit no compaction flags; got {args:?}"
        );
    }

    #[test]
    fn apply_compaction_flags_emits_threshold_when_set() {
        let mut cmd = Command::new("docker");
        let compaction = crate::dispatch::CompactionDispatchArgs {
            threshold_tokens: Some(35_000),
            ..Default::default()
        };
        apply_compaction_flags(&mut cmd, &compaction);
        let args = args_of(&cmd);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--compact-threshold-tokens" && w[1] == "35000"),
            "expected --compact-threshold-tokens 35000; got {args:?}"
        );
    }

    #[test]
    fn apply_compaction_flags_emits_all_when_set() {
        let mut cmd = Command::new("docker");
        let compaction = crate::dispatch::CompactionDispatchArgs {
            threshold_tokens: Some(45_000),
            compactor_model: Some("custom-compactor".to_string()),
            threshold_ratio: Some(0.35),
            context_window: Some(101_000),
            strategy: Some(darkmux_types::CompactionStrategy::StructuredSlot),
            bail_after_compactions: None,
            custom_instructions: None,
        };
        apply_compaction_flags(&mut cmd, &compaction);
        let args = args_of(&cmd);
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
        let mut cmd = Command::new("docker");
        let compaction = crate::dispatch::CompactionDispatchArgs {
            bail_after_compactions: Some(3),
            custom_instructions: None,
            ..Default::default()
        };
        apply_compaction_flags(&mut cmd, &compaction);
        let args = args_of(&cmd);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--bail-after-compactions" && w[1] == "3"),
            "expected --bail-after-compactions 3; got {args:?}"
        );
    }

    #[test]
    fn apply_compaction_flags_omits_bail_when_none() {
        let mut cmd = Command::new("docker");
        let compaction = crate::dispatch::CompactionDispatchArgs::default();
        apply_compaction_flags(&mut cmd, &compaction);
        let args = args_of(&cmd);
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
        let mut cmd = Command::new("docker");
        let compaction = crate::dispatch::CompactionDispatchArgs {
            custom_instructions: Some(
                "Preserve verbatim X / list active files with what was learned".into(),
            ),
            ..Default::default()
        };
        apply_compaction_flags(&mut cmd, &compaction);
        let args = args_of(&cmd);
        assert!(
            args.windows(2).any(|w| w[0] == "--compactor-custom-instructions"
                && w[1] == "Preserve verbatim X / list active files with what was learned"),
            "expected --compactor-custom-instructions with operator text; got {args:?}"
        );
    }

    #[test]
    fn apply_compaction_flags_omits_custom_instructions_when_none() {
        let mut cmd = Command::new("docker");
        let compaction = crate::dispatch::CompactionDispatchArgs::default();
        apply_compaction_flags(&mut cmd, &compaction);
        let args = args_of(&cmd);
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
        let mut cmd = Command::new("docker");
        let compaction = crate::dispatch::CompactionDispatchArgs {
            strategy: Some(darkmux_types::CompactionStrategy::StructuredSlot),
            ..Default::default()
        };
        apply_compaction_flags(&mut cmd, &compaction);
        let args = args_of(&cmd);
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
            description: None,
            default_model: None,
            models: vec![ProfileModel {
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
            description: None,
            default_model: None,
            models: vec![ProfileModel {
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
            description: None,
            default_model: None,
            models: vec![ProfileModel {
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
            description: None,
            default_model: None,
            models: vec![ProfileModel {
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
            description: None,
            default_model: None,
            models: vec![ProfileModel {
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
            description: None,
            default_model: None,
            models: vec![ProfileModel {
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
            description: None,
            default_model: None,
            models: vec![ProfileModel {
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
            description: None,
            default_model: None,
            models: vec![ProfileModel {
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
            description: None,
            default_model: None,
            models: vec![ProfileModel {
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
        assert_eq!(inactivity_timeout_seconds(), DEFAULT_INACTIVITY_TIMEOUT_SECS);
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
        assert_eq!(inactivity_timeout_seconds(), DEFAULT_INACTIVITY_TIMEOUT_SECS);
        unsafe { std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS") };
        if let Some(v) = prev {
            unsafe { std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v) };
        }
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
