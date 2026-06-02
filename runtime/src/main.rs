//! darkmux-runtime — container-bounded agent runtime binary.
//!
//! Composition:
//!
//! - LMStudio HTTP client (`lmstudio` module)
//! - Tool-call loop (`loop_runner` module)
//! - Tool palette: `search`, `read`, `edit`, `write`, `bash`, `echo`
//!   (echo retained for sanity-check unit tests; not in the production
//!   dispatch palette — see `runtime/src/main.rs::run_dispatch`)
//! - Token-count-aware compaction (`compaction` module)
//! - Per-dispatch trajectory + metrics recorder (`trajectory` module)
//!
//! Subcommands:
//!
//! - `--check`            → container environment probe
//! - `--version`          → version
//! - `run --model <id> --system <text> --prompt <text>` →
//!   run a single tool-call loop to completion; print the final
//!   assistant message + metrics
//!
//! See `README.md` for the architectural context.

use std::env;
use std::path::Path;
use std::process::ExitCode;

mod compaction;
mod cycle_detector;
mod failure_rate;
mod feedback;
mod json_repair;
mod lmstudio;
mod loop_runner;
mod plain_text_tool_calls;
mod reasoning_loop;
mod tools;
mod trajectory;

use lmstudio::{LmStudioClient, Message};
use tools::Tool;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let subcommand = args.get(1).map(String::as_str);

    match subcommand {
        Some("--check") | Some("check") => run_check(),
        Some("--version") | Some("version") => {
            println!("darkmux-runtime {VERSION}");
            ExitCode::SUCCESS
        }
        Some("run") => run_dispatch(&args[2..]),
        Some(other) => {
            eprintln!("unknown subcommand: {other}");
            eprintln!("known: --check, --version, run");
            ExitCode::from(2)
        }
        None => {
            println!("darkmux-runtime {VERSION}");
            println!();
            println!("Usage:");
            println!("  darkmux-runtime --check");
            println!("  darkmux-runtime --version");
            println!("  darkmux-runtime run --model <id> --system <text> --prompt <text>");
            println!("    [--no-stream] [--json] [--allowed-tools csv]");
            println!("    [--compact-threshold-tokens N] [--compactor-model id]");
            println!("    [--compact-threshold-ratio 0.1-0.9] [--context-window N]");
            println!("    [--compact-strategy narrative|structured-slot]");
            println!("    [--bail-after-compactions N]");
            println!();
            println!("Flags:");
            println!("  --json       Emit structured envelope on stdout (status to stderr).");
            println!("               Schema: {{ result, final_assistant, metrics, trajectory_path }}");
            ExitCode::SUCCESS
        }
    }
}

/// Container environment probe. First thing a dispatch can run to
/// verify the workspace mount + container layout are sound.
fn run_check() -> ExitCode {
    let mut all_ok = true;

    println!("darkmux-runtime {VERSION} — container environment check");
    println!();

    let workspace = Path::new("/workspace");
    if workspace.is_dir() {
        let writable = test_workspace_writable(workspace);
        if writable {
            println!("[ok]  /workspace exists and is writable");
        } else {
            println!("[!!]  /workspace exists but is NOT writable by this user");
            all_ok = false;
        }
    } else {
        println!("[!!]  /workspace does NOT exist (was the volume mounted?)");
        all_ok = false;
    }

    println!("[..]  effective USER env: {}", env::var("USER").unwrap_or("<unset>".into()));
    println!("[..]  PATH: {}", env::var("PATH").unwrap_or_default());

    println!();
    if all_ok {
        println!("phase 1 sanity: PASS");
        ExitCode::SUCCESS
    } else {
        println!("phase 1 sanity: one or more checks failed");
        ExitCode::from(1)
    }
}

fn test_workspace_writable(workspace: &Path) -> bool {
    let probe = workspace.join(".darkmux-runtime-write-probe");
    if std::fs::write(&probe, b"runtime write probe").is_err() {
        return false;
    }
    let _ = std::fs::remove_file(&probe);
    true
}

/// `darkmux-runtime run --model <id> --system <text> --prompt <text>` driver.
///
/// Parses flags by hand to keep zero clap dependency in the runtime.
/// Will gain a proper parser in Phase 4.
fn run_dispatch(args: &[String]) -> ExitCode {
    let mut model: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut system: Option<String> = None;
    let mut base_url: Option<String> = None;
    // Streaming is on by default (#205). Operators / tests pass
    // `--no-stream` to fall back to the Phase 2 single-shot path —
    // useful for deterministic benchmarks or when a runtime regression
    // is suspected to involve the streaming layer specifically.
    let mut streaming: bool = true;
    // `--json` flips stdout to a single structured envelope at end of
    // dispatch (Sprint-A). Default stays human-readable for direct CLI
    // use; JSON is opt-in for consumers like the qa-review skill that
    // need machine-parseable output. All progress/status lines go to
    // stderr when JSON is set so stdout is clean for `jq`.
    let mut json_mode: bool = false;
    // `--allowed-tools <comma-separated-names>` filters the runtime's
    // hardcoded tool catalog to a subset (computed dispatcher-side from
    // the role's tool_palette.allow minus tool_palette.deny). When
    // absent, the runtime exposes the full catalog (back-compat).
    //
    // The catalog IS the capability surface — the model can only call
    // tools that appear in the `tools[]` field of the chat-completions
    // request. Filtering here ensures denied tools never reach the
    // model, regardless of whether the model follows its system-prompt
    // doctrine. Pre-filter, a model could ignore "you must not edit"
    // in the system prompt and call `edit` anyway because the tool
    // existed in the catalog.
    let mut allowed_tools: Option<Vec<String>> = None;
    // (#368 / #482) Compaction config flags — explicit values from the
    // host (which derives them from `profile.runtime.compaction.*` in
    // the typed schema landed in #357). The runtime requires at least
    // one of `--context-window` or `--compact-threshold-tokens` per
    // #482's `validate_compaction_cli_inputs`; the absolute fallback
    // const that pre-#482 covered the all-None case is gone. Env vars
    // are NOT consulted.
    let mut compact_threshold_tokens: Option<u32> = None;
    let mut compactor_model: Option<String> = None;
    // (#368) Formula-based trigger: max_history_share fraction +
    // loaded context_window. Mirrors openclaw's `maxHistoryShare`.
    // Both must be set for the formula trigger to activate; either
    // missing → formula disabled, absolute-threshold-only behavior.
    let mut compact_threshold_ratio: Option<f32> = None;
    let mut context_window: Option<u32> = None;
    // (#372 T2-C) Compaction strategy override. When None, runtime
    // uses default Narrative; operator opts into tier-2 by setting
    // `profile.runtime.compaction.strategy: "structured-slot"` which
    // host plumbs via `--compact-strategy structured-slot`.
    let mut compact_strategy: Option<compaction::CompactionStrategy> = None;
    // (#377) Escalation bound — after this many compactions, the
    // runtime exits with TerminalReason::EscalationTriggered. Host
    // plumbs from `profile.runtime.compaction.reserve.bail_after_compactions`
    // (typed field landed in #357; consumer is #377). None = unbounded.
    let mut bail_after_compactions: Option<u32> = None;
    // (#383) Operator-tunable custom instructions for the compactor.
    // Host plumbs from `profile.runtime.compaction.custom_instructions`
    // (typed field, schema-isolation doctrine). None = no augmentation.
    let mut compactor_custom_instructions: Option<String> = None;

    // (#457) Operator-opt-in caps on per-dispatch turn count + cumulative
    // completion tokens. Host derives from `DARKMUX_RUNTIME_MAX_TURNS` /
    // `DARKMUX_RUNTIME_MAX_TOKENS` env vars. Both default unlimited;
    // the inactivity timeout (#458) catches the genuine-stuck case.
    let mut max_turns: Option<u32> = None;
    let mut max_tokens: Option<u32> = None;

    // (#457 Step 2) Per-role feedback-template overrides. Dispatcher
    // serializes Role.feedback_templates to JSON; runtime parses into
    // a BTreeMap<signal_kind, template_string>. Empty/absent = the
    // FeedbackInjector uses its hardcoded defaults.
    let mut feedback_templates: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--model" => {
                if let Some(v) = args.get(i + 1) {
                    model = Some(v.clone());
                    i += 2;
                } else {
                    eprintln!("--model requires a value");
                    return ExitCode::from(2);
                }
            }
            "--prompt" => {
                if let Some(v) = args.get(i + 1) {
                    prompt = Some(v.clone());
                    i += 2;
                } else {
                    eprintln!("--prompt requires a value");
                    return ExitCode::from(2);
                }
            }
            "--system" => {
                if let Some(v) = args.get(i + 1) {
                    system = Some(v.clone());
                    i += 2;
                } else {
                    eprintln!("--system requires a value");
                    return ExitCode::from(2);
                }
            }
            "--base-url" => {
                if let Some(v) = args.get(i + 1) {
                    base_url = Some(v.clone());
                    i += 2;
                } else {
                    eprintln!("--base-url requires a value");
                    return ExitCode::from(2);
                }
            }
            "--no-stream" => {
                streaming = false;
                i += 1;
            }
            "--json" => {
                json_mode = true;
                i += 1;
            }
            "--allowed-tools" => {
                if let Some(v) = args.get(i + 1) {
                    let names: Vec<String> = v
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    allowed_tools = Some(names);
                    i += 2;
                } else {
                    eprintln!("--allowed-tools requires a comma-separated list");
                    return ExitCode::from(2);
                }
            }
            "--compact-threshold-tokens" => {
                if let Some(v) = args.get(i + 1) {
                    match v.parse::<u32>() {
                        Ok(n) => {
                            compact_threshold_tokens = Some(n);
                            i += 2;
                        }
                        Err(_) => {
                            eprintln!(
                                "--compact-threshold-tokens requires a positive integer (got: {v})"
                            );
                            return ExitCode::from(2);
                        }
                    }
                } else {
                    eprintln!("--compact-threshold-tokens requires a value");
                    return ExitCode::from(2);
                }
            }
            "--compactor-model" => {
                if let Some(v) = args.get(i + 1) {
                    compactor_model = Some(v.clone());
                    i += 2;
                } else {
                    eprintln!("--compactor-model requires a value");
                    return ExitCode::from(2);
                }
            }
            "--compact-threshold-ratio" => {
                if let Some(v) = args.get(i + 1) {
                    match v.parse::<f32>() {
                        Ok(f) if (0.1..=0.9).contains(&f) => {
                            compact_threshold_ratio = Some(f);
                            i += 2;
                        }
                        Ok(f) => {
                            eprintln!(
                                "--compact-threshold-ratio must be in range 0.1-0.9 (got: {f})"
                            );
                            return ExitCode::from(2);
                        }
                        Err(_) => {
                            eprintln!(
                                "--compact-threshold-ratio requires a fraction in 0.1-0.9 (got: {v})"
                            );
                            return ExitCode::from(2);
                        }
                    }
                } else {
                    eprintln!("--compact-threshold-ratio requires a value");
                    return ExitCode::from(2);
                }
            }
            "--context-window" => {
                if let Some(v) = args.get(i + 1) {
                    match v.parse::<u32>() {
                        Ok(n) => {
                            context_window = Some(n);
                            i += 2;
                        }
                        Err(_) => {
                            eprintln!(
                                "--context-window requires a positive integer (got: {v})"
                            );
                            return ExitCode::from(2);
                        }
                    }
                } else {
                    eprintln!("--context-window requires a value");
                    return ExitCode::from(2);
                }
            }
            "--compact-strategy" => {
                if let Some(v) = args.get(i + 1) {
                    match compaction::CompactionStrategy::from_cli_str(v) {
                        Ok(s) => {
                            compact_strategy = Some(s);
                            i += 2;
                        }
                        Err(msg) => {
                            eprintln!("--compact-strategy: {msg}");
                            return ExitCode::from(2);
                        }
                    }
                } else {
                    eprintln!(
                        "--compact-strategy requires a value (`narrative` or `structured-slot`)"
                    );
                    return ExitCode::from(2);
                }
            }
            "--bail-after-compactions" => {
                if let Some(v) = args.get(i + 1) {
                    match v.parse::<u32>() {
                        Ok(n) => {
                            bail_after_compactions = Some(n);
                            i += 2;
                        }
                        Err(_) => {
                            eprintln!(
                                "--bail-after-compactions requires a positive integer (got: {v})"
                            );
                            return ExitCode::from(2);
                        }
                    }
                } else {
                    eprintln!("--bail-after-compactions requires a value");
                    return ExitCode::from(2);
                }
            }
            "--compactor-custom-instructions" => {
                if let Some(v) = args.get(i + 1) {
                    compactor_custom_instructions = Some(v.clone());
                    i += 2;
                } else {
                    eprintln!("--compactor-custom-instructions requires a value");
                    return ExitCode::from(2);
                }
            }
            "--max-turns" => {
                if let Some(v) = args.get(i + 1) {
                    match v.parse::<u32>() {
                        Ok(n) => {
                            max_turns = Some(n);
                            i += 2;
                        }
                        Err(_) => {
                            eprintln!("--max-turns requires a positive integer (got: {v})");
                            return ExitCode::from(2);
                        }
                    }
                } else {
                    eprintln!("--max-turns requires a value");
                    return ExitCode::from(2);
                }
            }
            "--max-tokens" => {
                if let Some(v) = args.get(i + 1) {
                    match v.parse::<u32>() {
                        Ok(n) => {
                            max_tokens = Some(n);
                            i += 2;
                        }
                        Err(_) => {
                            eprintln!("--max-tokens requires a positive integer (got: {v})");
                            return ExitCode::from(2);
                        }
                    }
                } else {
                    eprintln!("--max-tokens requires a value");
                    return ExitCode::from(2);
                }
            }
            "--feedback-templates-json" => {
                if let Some(v) = args.get(i + 1) {
                    match serde_json::from_str::<std::collections::BTreeMap<String, String>>(v) {
                        Ok(map) => {
                            feedback_templates = map;
                            i += 2;
                        }
                        Err(e) => {
                            eprintln!(
                                "--feedback-templates-json failed to parse: {e}. \
                                 Expected JSON object of {{signal_kind: template_string}}. \
                                 Ignoring; runtime will use defaults. (#457 Step 2)"
                            );
                            i += 2;
                        }
                    }
                } else {
                    eprintln!("--feedback-templates-json requires a JSON-string value");
                    return ExitCode::from(2);
                }
            }
            other => {
                eprintln!("unknown flag: {other}");
                return ExitCode::from(2);
            }
        }
    }

    let model = match model {
        Some(m) => m,
        None => {
            eprintln!("--model is required (e.g. darkmux:qwen3.6-35b-a3b-turboquant-mlx)");
            return ExitCode::from(2);
        }
    };

    let prompt = match prompt {
        Some(p) => p,
        None => {
            eprintln!("--prompt is required");
            return ExitCode::from(2);
        }
    };

    // System prompt: a default that names the runtime + names the
    // available tools. Real dispatches override this via --system with
    // the role's .md prompt (see darkmux's crew dispatch path).
    let system_prompt = system.unwrap_or_else(|| {
        "You are running inside the darkmux-runtime container. \
         You have access to six tools:\n\
         \n\
         - `echo`   — echoes its `text` argument back (sanity check)\n\
         - `bash`   — runs a bash command with cwd=/workspace; returns exit + stdout + stderr\n\
         - `read`   — reads from a file inside /workspace; requires offset (1-indexed start line) and limit (max lines; 0 = read entire file from offset to end). Prefer specifying a small limit when you only need a region — pair with `search` to find the right offset. For multiple reads in one turn, emit multiple `read` tool_calls in one assistant response.\n\
         - `write`  — writes a NEW file (or fully replaces one) inside /workspace\n\
         - `edit`   — applies one or more targeted patches in a single call (edits[] array; each entry replaces old_string with new_string against the current file state); prefer this over `write` for modifications, and batch related changes into one call's edits[] array rather than emitting many edit calls\n\
         - `search` — finds a literal substring pattern in a file or directory tree, returning `path:line:content` matches. Use this to LOCATE text (function names, imports, error strings) before reading or editing — much cheaper than reading whole files when you only need to find specific identifiers\n\
         \n\
         All file paths must resolve inside /workspace. Paths that escape \
         (via .. or symlinks or absolute paths outside /workspace) are \
         rejected by the runtime. Use tools as needed; stop when the task \
         is done."
            .into()
    });

    let initial_messages = vec![
        Message::system(system_prompt),
        Message::user(prompt),
    ];

    let client = match base_url {
        Some(url) => LmStudioClient::with_base_url(url),
        None => LmStudioClient::new(),
    };

    // Tool order matters — LLMs have positional bias when picking between
    // plausible candidates. Order reflects the workflow we want the model
    // to consider: locate first (search), then read content, then modify
    // (edit/write), with bash as the general-purpose escape hatch.
    // `Tool::Echo` is excluded — it was a Phase 2 round-trip probe with
    // no use in real dispatches; sending it adds tool-catalog overhead
    // the model would never invoke.
    let full_catalog = [Tool::Search, Tool::Read, Tool::Edit, Tool::Write, Tool::Bash];
    let tools = filter_tools_by_allowed(&full_catalog, allowed_tools.as_deref());

    // Status lines go to stderr in JSON mode so stdout stays clean for
    // `jq`-style consumers. In human-readable mode they print to stdout
    // alongside the eventual final-message block.
    if json_mode {
        eprintln!("dispatching to model: {model}");
    } else {
        println!("dispatching to model: {model}");
        println!();
    }

    // (#482) Fail loud if neither the loaded model's context window nor
    // an explicit absolute threshold is known — pre-#482 the runtime
    // silently fell through to a hardcoded const, which made every
    // compaction decision downstream uncorrelated with the actual
    // model envelope. The standard host path (`darkmux crew dispatch`,
    // `darkmux lab run`) always supplies `--context-window` from the
    // primary model's `n_ctx`; this check catches direct callers that
    // bypass the host (or profiles missing a Primary model).
    //
    // Pre-flight ordering: validator runs BEFORE `Trajectory::open` so
    // a contract failure doesn't leave a dangling `dispatch_start`
    // record under `/workspace/.darkmux-runtime/trajectory.jsonl`
    // ("dispatch started → no further records" reads like a crash to
    // log scrapers).
    if let Err(msg) = compaction::validate_compaction_cli_inputs(
        compact_threshold_tokens,
        compact_threshold_ratio,
        context_window,
    ) {
        eprintln!("error: {msg}");
        return ExitCode::from(2);
    }

    // Open trajectory + metrics recorder against the mounted out-dir
    // (SEPARATE from /workspace so the runtime never writes its own
    // bookkeeping into the tree it's operating on). Phase 7: gives
    // post-dispatch visibility because the container is --rm and
    // otherwise everything except stderr is lost.
    let mut traj = trajectory::Trajectory::open(Path::new(trajectory::RUNTIME_OUT_BASE));
    let started_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let system_chars = initial_messages[0].content.as_deref().map(str::len).unwrap_or(0);
    let prompt_chars = initial_messages[1].content.as_deref().map(str::len).unwrap_or(0);
    traj.append_dispatch_start(&model, system_chars, prompt_chars);

    // (#368) Compaction config from explicit CLI args; no env-var
    // fallback (operator's tuning surface is the profile JSON, not
    // shell env). The host's `dispatch_via_internal` derives values
    // from `profile.runtime.compaction.*` and passes them as flags.
    let compaction_cfg = compaction::CompactionConfig::from_overrides_with_bail_and_custom(
        compact_threshold_tokens,
        compactor_model,
        compact_threshold_ratio,
        context_window,
        compact_strategy,
        bail_after_compactions,
        compactor_custom_instructions,
    );
    let run_result = loop_runner::run(
        &client,
        &model,
        initial_messages,
        &tools,
        &mut traj,
        streaming,
        &compaction_cfg,
        max_turns,
        max_tokens,
        feedback_templates,
    );

    let outcome = match run_result {
        Ok(o) => Some(o),
        Err(e) => {
            eprintln!("dispatch failed: {e:#}");
            None
        }
    };

    // Three-way result discrimination (#325):
    //   - Some(outcome) + terminal_reason=Stop  → "stop"
    //   - Some(outcome) + terminal_reason=MaxTurns → "max_turns"
    //   - None (loop errored) → "error"
    // Pre-fix MAX_TURNS was an Err indistinguishable from
    // infrastructure failures; structured terminal reason lets
    // downstream consumers (qa-review skill, lab adapter, future
    // heuristic engine) tell "model got stuck" from "container died."
    let result_str: &str = match outcome.as_ref() {
        Some(o) => match o.terminal_reason {
            loop_runner::TerminalReason::Stop => "stop",
            loop_runner::TerminalReason::MaxTurns => "max_turns",
            // (#377) The `result` field is operator-visible in the
            // JSON envelope; consumers (qa-review skill, lab adapter,
            // future heuristic engine) branch on it. New variants get
            // distinct snake-case strings so existing consumers can
            // add a case without grepping for hidden behavior.
            loop_runner::TerminalReason::EscalationTriggered(
                loop_runner::EscalationReason::CompactionLimitReached,
            ) => "escalation_compaction_limit_reached",
            loop_runner::TerminalReason::EscalationTriggered(
                loop_runner::EscalationReason::CumulativeTokensExceeded,
            ) => "escalation_cumulative_tokens_exceeded",
            loop_runner::TerminalReason::EscalationTriggered(
                loop_runner::EscalationReason::IntraTurnStallExhausted,
            ) => "escalation_intra_turn_stall_exhausted",
        },
        None => "error",
    };

    // Whether success or failure, write the trajectory close + metrics.
    let wall_ms = traj.elapsed_ms();
    traj.append_dispatch_complete(result_str, wall_ms);

    if let Some(o) = &outcome {
        let final_assistant = o
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .and_then(|m| m.content.clone())
            .unwrap_or_else(|| "<empty>".into());

        let preview: String = final_assistant.chars().take(400).collect();
        let max_turns_reached =
            matches!(o.terminal_reason, loop_runner::TerminalReason::MaxTurns);
        let metrics = trajectory::Metrics {
            runtime: "darkmux-runtime",
            version: VERSION,
            model: model.clone(),
            started_at_unix_ms,
            wall_ms,
            result: result_str.into(),
            turns: o.turns,
            compactions: o.compactions,
            total_prompt_tokens: o.total_prompt_tokens,
            total_completion_tokens: o.total_completion_tokens,
            total_messages: o.messages.len(),
            max_turns_reached,
            final_assistant_preview: preview,
        };
        let _ = traj.save_metrics(&metrics);

        if json_mode {
            let envelope = build_json_envelope(
                result_str,
                Some(&final_assistant),
                &model,
                started_at_unix_ms,
                wall_ms,
                o.turns,
                o.compactions,
                o.total_prompt_tokens,
                o.total_completion_tokens,
                o.messages.len(),
            );
            println!("{}", serde_json::to_string(&envelope).unwrap_or_else(|_| "{}".into()));
        } else {
            println!("--- final assistant message ---");
            println!("{final_assistant}");
            println!();
            println!("--- metrics ---");
            println!("turns:             {}", o.turns);
            println!("compactions:       {}", o.compactions);
            println!("prompt tokens:     {}", o.total_prompt_tokens);
            println!("completion tokens: {}", o.total_completion_tokens);
            println!("total messages:    {}", o.messages.len());
            println!("wall:              {wall_ms}ms");
        }
    } else {
        // Loop returned an error — still write a minimal metrics file
        // so the operator has a record of the failure.
        let metrics = trajectory::Metrics {
            runtime: "darkmux-runtime",
            version: VERSION,
            model: model.clone(),
            started_at_unix_ms,
            wall_ms,
            result: result_str.into(),
            turns: 0,
            compactions: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_messages: 0,
            max_turns_reached: true,
            final_assistant_preview: String::new(),
        };
        let _ = traj.save_metrics(&metrics);

        if json_mode {
            let envelope = build_json_envelope(
                "error", None, &model, started_at_unix_ms, wall_ms,
                0, 0, 0, 0, 0,
            );
            println!("{}", serde_json::to_string(&envelope).unwrap_or_else(|_| "{}".into()));
        }
        return ExitCode::from(1);
    }

    ExitCode::SUCCESS
}

/// Construct the `--json` envelope. Pure function — extracted so the
/// schema is tested independently of the dispatch shell-out (which
/// requires LMStudio + Docker). Same shape for success + error paths so
/// consumers (qa-review skill, lab harness adapter) parse uniformly.
///
/// `final_assistant = None` produces a JSON `null` for the field —
/// error envelopes use this; success envelopes always carry a string.
///
/// Arg types mirror the source types in `trajectory::Metrics` +
/// `loop_runner::Outcome` so callers don't need casts.
#[allow(clippy::too_many_arguments)]
fn build_json_envelope(
    result: &str,
    final_assistant: Option<&str>,
    model: &str,
    started_at_unix_ms: u64,
    wall_ms: u128,
    turns: u32,
    compactions: u32,
    prompt_tokens: u32,
    completion_tokens: u32,
    total_messages: usize,
) -> serde_json::Value {
    serde_json::json!({
        "result": result,
        "final_assistant": match final_assistant {
            Some(s) => serde_json::Value::String(s.to_string()),
            None => serde_json::Value::Null,
        },
        "metrics": {
            "runtime": "darkmux-runtime",
            "version": VERSION,
            "model": model,
            "started_at_unix_ms": started_at_unix_ms,
            // u128 wall_ms is safe to narrow for JSON numeric encoding —
            // u64 covers 584 million years of milliseconds.
            "wall_ms": wall_ms as u64,
            "turns": turns,
            "compactions": compactions,
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_messages": total_messages,
        },
        // Container-internal path where the runtime's own bookkeeping
        // landed — now the out-dir (SEPARATE from /workspace) per the
        // out-of-band bookkeeping change. Built from the shared
        // trajectory module constants so it can't drift from the actual
        // write site.
        "trajectory_path": trajectory::runtime_dir()
            .join("trajectory.jsonl")
            .display()
            .to_string(),
    })
}

/// Filter the runtime's tool catalog by an allow-list of tool names
/// (the role's tool_palette.allow minus tool_palette.deny, computed
/// dispatcher-side and passed via `--allowed-tools`).
///
/// When `allowed` is `None`, returns the full catalog (back-compat for
/// callers that haven't been updated to pass the flag).
/// When `allowed` is `Some(list)`, returns only tools whose `name()`
/// appears in the list.
///
/// **This is the runtime-side enforcement of role.tool_palette.deny.**
/// Before this enforcement existed, every role saw all runtime tools
/// regardless of its declared deny list — a model that ignored its
/// .md system prompt's "you must not edit" doctrine could still call
/// the edit tool because the tool existed in the catalog. With this
/// filter, denied tools are never in the catalog the LMStudio chat-
/// completions API sees, so the model cannot call them.
fn filter_tools_by_allowed(tools: &[Tool], allowed: Option<&[String]>) -> Vec<Tool> {
    match allowed {
        None => tools.to_vec(),
        Some(names) => tools
            .iter()
            .filter(|t| names.iter().any(|n| n == t.name()))
            .copied()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── tool catalog filter (runtime-side enforcement of role tool_palette) ──

    #[test]
    fn filter_tools_allowed_none_returns_full_catalog() {
        let full = [Tool::Search, Tool::Read, Tool::Edit, Tool::Write, Tool::Bash];
        let result = filter_tools_by_allowed(&full, None);
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn filter_tools_allowed_subset_returns_only_those_tools() {
        let full = [Tool::Search, Tool::Read, Tool::Edit, Tool::Write, Tool::Bash];
        let allow: Vec<String> = vec!["read".into(), "search".into(), "bash".into()];
        let result = filter_tools_by_allowed(&full, Some(&allow));
        assert_eq!(result.len(), 3, "expected 3 tools after filter, got {result:?}");
        assert!(
            !result.iter().any(|t| matches!(t, Tool::Edit | Tool::Write)),
            "filtered catalog must NOT include Edit or Write; got {result:?}"
        );
    }

    #[test]
    fn filter_tools_allowed_empty_list_returns_nothing() {
        let full = [Tool::Search, Tool::Read, Tool::Edit, Tool::Write, Tool::Bash];
        let allow: Vec<String> = vec![];
        let result = filter_tools_by_allowed(&full, Some(&allow));
        assert_eq!(
            result.len(),
            0,
            "empty allowed-list → empty filtered catalog; got {result:?}"
        );
    }

    #[test]
    fn filter_tools_allowed_unknown_names_silently_dropped() {
        let full = [Tool::Search, Tool::Read, Tool::Edit, Tool::Write, Tool::Bash];
        let allow: Vec<String> = vec!["nonexistent".into(), "made-up-tool".into()];
        let result = filter_tools_by_allowed(&full, Some(&allow));
        assert_eq!(
            result.len(),
            0,
            "unknown tool names must not match anything; got {result:?}"
        );
    }

    #[test]
    fn filter_tools_allowed_preserves_input_order() {
        let full = [Tool::Search, Tool::Read, Tool::Edit, Tool::Write, Tool::Bash];
        let allow: Vec<String> = vec!["bash".into(), "read".into()];
        let result = filter_tools_by_allowed(&full, Some(&allow));
        // Order should follow the input catalog (Read before Bash),
        // not the allow-list arg order. LMStudio's tool-call bias is
        // sensitive to position; the catalog order is the contract.
        assert_eq!(result.len(), 2);
        assert!(matches!(result[0], Tool::Read));
        assert!(matches!(result[1], Tool::Bash));
    }

    #[test]
    fn json_envelope_success_carries_final_assistant_and_metrics() {
        let env = build_json_envelope(
            "stop",
            Some("hello world"),
            "darkmux:qwen3.6-35b-a3b",
            1700000000000,
            2135,
            1,
            0,
            2970,
            112,
            3,
        );
        // Top-level contract — qa-review + lab adapter parse these.
        assert_eq!(env["result"], "stop");
        assert_eq!(env["final_assistant"], "hello world");
        assert_eq!(env["trajectory_path"], "/darkmux-out/.darkmux-runtime/trajectory.jsonl");
        // Metrics block — mirrors trajectory::Metrics field names so the
        // two surfaces stay aligned.
        assert_eq!(env["metrics"]["runtime"], "darkmux-runtime");
        assert_eq!(env["metrics"]["model"], "darkmux:qwen3.6-35b-a3b");
        assert_eq!(env["metrics"]["wall_ms"], 2135);
        assert_eq!(env["metrics"]["turns"], 1);
        assert_eq!(env["metrics"]["prompt_tokens"], 2970);
        assert_eq!(env["metrics"]["completion_tokens"], 112);
        assert_eq!(env["metrics"]["total_messages"], 3);
    }

    #[test]
    fn json_envelope_error_carries_null_final_assistant() {
        // Failure path emits same envelope shape so consumers can parse
        // uniformly without branching on success/error.
        let env = build_json_envelope(
            "error", None, "darkmux:foo", 1700000000000, 500, 0, 0, 0, 0, 0,
        );
        assert_eq!(env["result"], "error");
        assert!(env["final_assistant"].is_null(), "error envelope must have null final_assistant");
        assert_eq!(env["metrics"]["model"], "darkmux:foo");
        assert_eq!(env["metrics"]["wall_ms"], 500);
        assert_eq!(env["metrics"]["turns"], 0);
    }

    #[test]
    fn json_envelope_serializes_as_single_line() {
        // qa-review parses with `jq -c` — verify the serialized form is
        // single-line + valid JSON. (No surprise whitespace, etc.)
        let env = build_json_envelope("stop", Some("x"), "m", 0, 0, 0, 0, 0, 0, 0);
        let s = serde_json::to_string(&env).unwrap();
        assert!(!s.contains('\n'), "envelope must serialize on one line; got: {s}");
        // Round-trip must produce identical structure.
        let back: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn json_envelope_handles_final_assistant_with_special_chars() {
        // The final assistant message can contain newlines, quotes,
        // backslashes — serde_json must escape them so the envelope
        // stays parseable. Regression guard for "naive println escaping"
        // mistakes that would tempt a future refactor.
        let tricky = "line1\nline2\twith \"quotes\" and \\backslash";
        let env = build_json_envelope("stop", Some(tricky), "m", 0, 0, 0, 0, 0, 0, 0);
        let s = serde_json::to_string(&env).unwrap();
        let back: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back["final_assistant"], tricky);
    }
}
