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
//! - Per-dispatch trajectory recorder (`trajectory` module)
//!
//! Subcommands:
//!
//! - `--check`            → container environment probe
//! - `--version`          → version
//! - `run --model <id> --system <text> --prompt <text>` →
//!   run a single tool-call loop to completion; print the final
//!   assistant message and the run's counts (read back from its trajectory)
//!
//! See `README.md` for the architectural context.

use std::env;
use std::path::Path;
use std::process::ExitCode;

mod compaction;
mod compaction_repeat;
mod cycle_detector;
mod detection;
mod failure_rate;
mod feedback;
mod interrupt;
mod json_repair;
mod lmstudio;
mod messages_dialect;
mod checkpoint;
mod loop_runner;
mod pace;
mod plain_text_tool_calls;
mod bounds;
mod budget_request;
mod reasoning_loop;
mod stream_gate;
mod tool_result_prune;
mod unproductive_compactions;
mod tools;
mod trajectory;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod usage_conformance;

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
            println!("    [--compactor-context-window N]");
            println!("    [--compactor-chat-url url] [--compactor-dialect d] [--compactor-auth-header-stdin]");
            println!("    [--compact-strategy narrative|structured-slot]");
            println!("    [--bail-after-compactions N]");
            println!();
            println!("Flags:");
            println!("  --json       Emit structured envelope on stdout (status to stderr).");
            println!("               Schema: {{ result, final_assistant, trajectory_path }}");
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
fn run_dispatch(args: &[String]) -> ExitCode {
    let RunArgs {
        model,
        prompt,
        system,
        resume_checkpoint_path,
        role_id,
        execution_id,
        response_schema,
        endpoints,
        streaming,
        json_mode,
        allowed_tools,
        compact_threshold_tokens,
        compactor_model,
        compactor_context_window,
        compact_threshold_ratio,
        context_window,
        compact_strategy,
        bail_after_compactions,
        compactor_custom_instructions,
        max_turns,
        max_tokens,
        max_tokens_per_call,
        reasoning_checkpoint_interval,
        generation_checkpoint_interval,
        max_stall_recoveries,
        max_tokens_per_call_source,
        reasoning_checkpoint_interval_source,
        generation_checkpoint_interval_source,
        feedback_templates,
    } = match parse_run_args(args, std::env::var("DARKMUX_RESUME_CHECKPOINT").ok()) {
        Ok(parsed) => parsed,
        Err(e) => return e.exit(),
    };
    let role_id = role_id.unwrap_or_default();

    let (model, prompt) = match required_model_and_prompt(model, prompt) {
        Ok(required) => required,
        Err(e) => return e.exit(),
    };

    let system_prompt = system.unwrap_or_else(default_system_prompt);

    // (#2386) The finding-key identity this dispatch mints under, plus the
    // brief it was given. `create_finding` returns the real key
    // (`<execution-id>/<seq>`) so the model has the address to name in a
    // `create_mod`'s `for`, and `create_mod` can refuse a key that names
    // neither a finding this run recorded nor one this brief handed it —
    // the shape check alone accepted the tool description's own example,
    // which a reviewer seat then copied onto six mods that linked to
    // nothing. Set BEFORE the loop starts, from the values the host passed.
    crate::tools::set_dispatch_context(execution_id.clone(), &prompt);

    let initial_messages = vec![
        Message::system(system_prompt),
        Message::user(prompt),
    ];

    // (#2114) Load the checkpoint (if `--resume`/`DARKMUX_RESUME_CHECKPOINT`
    // named one) BEFORE the loop runs — `initial_messages` above is built
    // unconditionally (the host always supplies `--system`/`--prompt`) but
    // gets discarded by `loop_runner::run` in favor of the checkpoint's own
    // history when `resume_from` is `Some`. A missing/corrupt checkpoint
    // fails the dispatch loudly here, before anything else runs, rather
    // than silently starting fresh under a name that looked like a resume.
    let resume_from = match load_resume_checkpoint(resume_checkpoint_path.as_deref()) {
        Ok(resume_from) => resume_from,
        Err(e) => return e.exit(),
    };
    let resumed_from_turn_index = resume_from.as_ref().map(|c| c.turns);

    // (#1187 audit finding) Build the compactor's client from `base_url`,
    // and NEVER apply `--chat-url`/
    // `--auth-header-stdin` to it — the compactor always talks to local
    // LMStudio, even on a remote-brain dispatch. See the doc comment on
    // `loop_runner::run`'s `compactor_client` parameter for why routing
    // the compactor through a remote-configured client is wrong, not just
    // different (it silently burns the wrong model's budget on Azure, or
    // 404s and fails the whole dispatch on OpenAI-style endpoints).
    //
    // (#3207) The one exception: a utility model on an UNMANAGED endpoint
    // (`--compactor-chat-url`), which is the compactor's OWN endpoint, never
    // the brain's, with its own dialect and its own auth header.
    //
    // (#1187) Auth headers are read from stdin ONCE at startup — never a CLI
    // arg (ps-visible), never an env var (visible to every process, no
    // permission gate), never a file (bash has no `/workspace`-escape check,
    // so any secret-bearing file the container can see is reachable by a
    // model-issued `cat` at any point during the run — stdin leaves no such
    // FILESYSTEM artifact, since nothing is ever written to disk; see the
    // longer note on `auth_header_stdin`'s declaration for the residual
    // in-memory exposure this does NOT close). A parse/read failure is
    // fatal (exit 2): a remote-brain dispatch with a broken auth pipe would
    // otherwise proceed unauthenticated and fail confusingly on the FIRST
    // model call instead of loudly at startup. (#3207) The brain's header and
    // the compactor's share the one blob (`read_stdin_auth`).
    let (client, compactor_client) = match build_clients(endpoints) {
        Ok(clients) => clients,
        Err(e) => return e.exit(),
    };

    // Tool order matters — LLMs have positional bias when picking between
    // plausible candidates. Order reflects the workflow we want the model
    // to consider: locate first (search), then read content, then modify
    // (edit/write), with bash as the general-purpose escape hatch.
    // `Tool::Echo` is excluded — it was a Phase 2 round-trip probe with
    // no use in real dispatches; sending it adds tool-catalog overhead
    // the model would never invoke.
    let full_catalog = [Tool::Search, Tool::Read, Tool::Edit, Tool::Write, Tool::Bash];
    let tools = advertised_tools(&full_catalog, allowed_tools.as_deref());
    // (#2268) The advertised set is a FACT about this run and belongs in
    // the artifact: the crawler's `create_finding` was silently absent from
    // every request for two releases, and the first place that was visible
    // was the model's own reasoning ("The tools I have are: search, read,
    // bash"). Recorded on `dispatch.start` so the next such gap is a grep.
    let tool_names: Vec<&str> = tools.iter().map(|t| t.name()).collect();

    // (Security audit, #2114 resume follow-up) Refuses a CROSS-ROLE or
    // CROSS-PROMPT resume — a checkpoint written under a different role,
    // or a different system prompt than the one THIS run was launched
    // with. It does NOT defend against a SAME-ROLE forgery: a resume
    // replays `pending_tool_calls` verbatim (name checked against this
    // run's own tool allowlist, arguments never validated), and restores
    // every prior assistant/tool message from the file with zero
    // validation at all. `/darkmux-out` stays read-write even when
    // `/workspace` is `:ro` (crawl-kind dispatches), so a prompt-injected
    // model can still get a same-role checkpoint's tool arguments — or
    // its non-pending history — to say whatever it wants, and a later
    // `--resume-from` under the SAME role replays it. Checkpoint
    // integrity (making the file itself untamperable by the model it's
    // checkpointing) is a follow-up: the real fix is uid separation
    // between the runtime process and the tool-executing processes
    // inside the container, so a tool call can never write the runtime's
    // own checkpoint file. See `checkpoint::validate_for_resume`'s own
    // doc for the full accounting, and `--resume-from`'s CLI help
    // (`src/cli.rs`, host side) for the operator-facing version of this
    // warning. The system-message comparison is against the fresh one
    // main.rs just built, at `initial_messages[0]` (never the file's own
    // claim) — compaction never rewrites it: `compaction::PRESERVE_HEAD`
    // keeps index 0 untouched across every compaction generation. A
    // THIRD leg (workspace mount-mode/path) is closed entirely host-side
    // — this runtime process has no visibility into it at all; see
    // `dispatch_internal::stage_resume_checkpoint`'s own doc.
    if let Some(checkpoint) = &resume_from {
        let allowed_tool_names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let fresh_system_message = initial_messages[0].content.as_deref().unwrap_or("");
        if let Err(e) =
            checkpoint::validate_for_resume(checkpoint, &allowed_tool_names, fresh_system_message)
        {
            eprintln!("--resume {}: {e:#}", resume_checkpoint_path.as_deref().unwrap_or(""));
            return ExitCode::from(2);
        }
    }

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
    // model envelope. The standard host path (`darkmux dispatch`,
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

    // Open the trajectory recorder against the mounted out-dir (SEPARATE
    // from /workspace so the runtime never writes its own bookkeeping into
    // the tree it's operating on). The container is --rm, so this is what
    // survives it.
    let mut traj = trajectory::Trajectory::open(&trajectory::out_base());
    // (#3014) A SIGTERM/SIGINT closes the trajectory with `interrupted`.
    interrupt::install_for(&traj);
    let system_chars = initial_messages[0].content.as_deref().map(str::len).unwrap_or(0);
    let prompt_chars = initial_messages[1].content.as_deref().map(str::len).unwrap_or(0);
    traj.append_dispatch_start(&model, system_chars, prompt_chars, &tool_names);

    // (#368) Compaction config from explicit CLI args; no env-var
    // fallback (operator's tuning surface is the profile JSON, not
    // shell env). The host's `dispatch_via_internal` derives values
    // from `profile.runtime.compaction.*` and passes them as flags.
    let compaction_cfg = compaction::CompactionConfig::from_overrides_with_bail_and_custom(
        compact_threshold_tokens,
        compactor_model,
        compact_threshold_ratio,
        compaction::CompactionWindows {
            primary: context_window.map(compaction::PrimaryWindow),
            compactor: compactor_context_window.map(compaction::CompactorWindow),
        },
        compact_strategy,
        bail_after_compactions,
        compactor_custom_instructions,
    );
    // (MUST FIX 1, #2571 follow-up) Disclose at startup, loudly, when this
    // dispatch is about to run with no compactor bound but a real
    // compaction trigger configured — either a context window (the
    // formula/window-derived trigger) or an absolute
    // `--compact-threshold-tokens` with no window at all. Before this there
    // was no signal anywhere — no message, no trajectory event, no flow
    // record, no envelope field — so a zero compaction count read
    // identically to "never needed one."
    if let Some(msg) = compaction::compactor_disclosure_message(&compaction_cfg) {
        eprintln!("{msg}");
    }
    // (#1038) Wrap the role's output schema (--response-schema) into an LMStudio
    // json_schema response_format so every model turn is grammar-constrained to
    // that shape. Invalid/absent schema ⇒ None ⇒ free-form (today's behavior).
    // `strict: true` is deliberate and DIFFERS from the compactor's
    // `structured_output_response_format_schema()` (strict: false, which keeps
    // forward-compat tolerance for unknown fields): a role's output feeds a
    // downstream parser that wants a hard contract, so the schema is authored
    // strict-safe (every property in `required`, `additionalProperties: false`,
    // optionals nullable via `anyOf` — NOT the `"type":["string","null"]` union,
    // which LMStudio's grammar compiler rejects with "'type' must be a string").
    // Don't "unify" the two paths — the divergence is by use case.
    let response_format = response_schema.as_deref().and_then(|s| {
        serde_json::from_str::<serde_json::Value>(s).ok()
    }).map(|schema| {
        serde_json::json!({
            "type": "json_schema",
            "json_schema": { "name": "role_output", "strict": true, "schema": schema }
        })
    });
    // (#2165) Stamp the resolved bound provenance ONCE, before the loop
    // starts — every emission site downstream reads it back via
    // `bounds::bound_sources()` rather than re-deriving it.
    bounds::set_bound_sources(bounds::BoundSources {
        reasoning_checkpoint_interval: bounds::BoundSource::from_cli_str(
            reasoning_checkpoint_interval_source.as_deref().unwrap_or(""),
        ),
        max_tokens_per_call: bounds::BoundSource::from_cli_str(
            max_tokens_per_call_source.as_deref().unwrap_or(""),
        ),
        // (#2171 rebase onto #2165) Same companion-source pattern, for the
        // generation check-in.
        generation_checkpoint_interval: bounds::BoundSource::from_cli_str(
            generation_checkpoint_interval_source.as_deref().unwrap_or(""),
        ),
    });
    let run_result = loop_runner::run_resumable(
        &client,
        &compactor_client,
        &model,
        initial_messages,
        &tools,
        &mut traj,
        streaming,
        &compaction_cfg,
        max_turns,
        max_tokens,
        max_tokens_per_call,
        reasoning_checkpoint_interval,
        generation_checkpoint_interval,
        max_stall_recoveries,
        feedback_templates,
        response_format,
        // (#2114 finding 3) pace.json / checkpoint.json live in the
        // out-dir mount, not the workspace — see `loop_runner::
        // run_resumable`'s doc on this param for why.
        &trajectory::out_base(),
        &role_id,
        resume_from,
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
            // (#377) The `result` field is operator-visible in the JSON
            // envelope; consumers (qa-review skill, lab adapter, future
            // heuristic engine) branch on it. (#2190) Delegates to
            // `loop_runner::escalation_reason_str` — the single source of
            // truth for the mapping, so the envelope's `result` and the
            // `dispatch.escalation.triggered` trajectory event's `reason`
            // can never drift apart.
            loop_runner::TerminalReason::EscalationTriggered(reason) => {
                loop_runner::escalation_reason_str(reason)
            }
        },
        None => "error",
    };

    // Whether success or failure, close the trajectory. Every count of this
    // execution (turns, tokens, rests) is in the trajectory already, written
    // as it happened; the host folds it, so nothing here restates a total.
    let wall_ms = traj.elapsed_ms();
    traj.append_dispatch_complete(
        result_str,
        wall_ms,
        outcome.as_ref().map(|o| o.turn_delay_effective_ms),
    );

    if let Some(o) = &outcome {
        // (#1221) Prefer the answer the LOOP identified. "Last assistant
        // message" is the checkpoint prefill on every non-terminal exit, so
        // re-deriving it here handed the operator the model's raw scratch work
        // as its answer.
        let final_assistant = o
            .final_answer
            .clone()
            .filter(|a| !a.trim().is_empty())
            .or_else(|| {
                o.messages
                    .iter()
                    .rev()
                    .find(|m| m.role == "assistant")
                    .and_then(|m| m.content.clone())
            })
            .unwrap_or_else(|| "<empty>".into());

        if json_mode {
            let mut envelope = build_json_envelope(result_str, Some(&final_assistant));
            // (#799) Stamp the verifier-fabrication backstop: the bash commands
            // that FAILED TO RUN this dispatch. The gate cross-checks a SIGNOFF's
            // verification claims against this; empty on an honest run.
            envelope.failed_tool_invocations = Some(o.failed_to_run.clone());
            // (#2114) `resumed_from`: path + the checkpoint's OWN turn
            // index (where the dispatch resumed FROM, not where it ended
            // up), present only when this dispatch actually reloaded a
            // checkpoint.
            if let (Some(path), Some(turn_index)) = (&resume_checkpoint_path, resumed_from_turn_index) {
                envelope.resumed_from = Some(darkmux_trajectory::ResumedFrom { path: path.clone(), turn_index: u64::from(turn_index) });
            }
            println!("{}", serde_json::to_string(&envelope).unwrap_or_else(|_| "{}".into()));
        } else {
            println!("--- final assistant message ---");
            println!("{final_assistant}");
            println!();
            print_run_summary(result_str, wall_ms);
        }
    } else {
        if json_mode {
            let envelope = build_json_envelope("error", None);
            println!("{}", serde_json::to_string(&envelope).unwrap_or_else(|_| "{}".into()));
        } else {
            // (#905) Human mode was silent on failure while the success path
            // is verbose — print a failure summary so a non-JSON dispatch
            // doesn't just vanish with a bare exit code.
            println!("--- dispatch failed ---");
            println!("model:  {model}");
            print_run_summary(result_str, wall_ms);
        }
        return ExitCode::from(1);
    }

    if let Some(o) = &outcome {
        if matches!(o.terminal_reason, loop_runner::TerminalReason::EscalationTriggered(_)) {
            return ExitCode::from(1);
        }
    }

    ExitCode::SUCCESS
}

/// System prompt: a default that names the runtime + names the
/// available tools. Real dispatches override this via --system with
/// the role's .md prompt (see darkmux's dispatch path). Model-facing text
/// (contract 6): moved here verbatim, never reworded.
fn default_system_prompt() -> String {
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
}

/// The flags of `darkmux-runtime run`, as parsed by [`parse_run_args`].
///
/// Parsed by hand to keep zero clap dependency in the runtime.
#[derive(Debug, Default)]
struct RunArgs {
    model: Option<String>,
    prompt: Option<String>,
    system: Option<String>,
    // (#2114) Path to a `checkpoint.json` (under the out-dir mount,
    // `/darkmux-out` in production) written by a prior,
    // interrupted dispatch. When set, the loop reloads the checkpoint's
    // message history instead of starting from `--system`/`--prompt` — see
    // `checkpoint::read_checkpoint`. `--resume` takes precedence over the
    // env var when both are present (flag wins, matching the CLI-over-env
    // precedence every other runtime flag/env pair in this file follows).
    resume_checkpoint_path: Option<String>,
    // (v3 checkpoint schema, security audit, #2114 resume follow-up) The
    // role id THIS dispatch is running as — host-supplied, since the
    // runtime has no other way to know it. Stamped into every
    // `checkpoint.json` write so a LATER `--resume-from` can refuse,
    // host-side, to resume a checkpoint recorded under a different role
    // (`dispatch_internal::stage_resume_checkpoint`). NOT required for a
    // fresh (non-resuming) dispatch to run — an absent/empty value just
    // means a future resume attempt's role comparison will (correctly)
    // never match anything but another equally-unlabeled checkpoint.
    // Absent reads as the empty string.
    role_id: Option<String>,
    // (#2386) The host's session id — the `<dispatch>` half of every
    // finding key this dispatch's `create_finding` calls will be
    // stored under (`dispatch_internal.rs::materialize_finding`
    // builds `<execution_id>/<emit_seq>`). Optional: a host that does
    // not pass it leaves the runtime unable to name a key, and both
    // tools fall back to their pre-#2386 behavior rather than
    // guessing one.
    execution_id: Option<String>,
    // (#1038) Raw JSON-schema string for the role's output, wrapped into an
    // LMStudio json_schema response_format before the loop. None ⇒ free-form.
    response_schema: Option<String>,
    endpoints: EndpointArgs,
    // Streaming is on by default (#205). Operators / tests pass
    // `--no-stream` to fall back to the Phase 2 single-shot path —
    // useful for deterministic benchmarks or when a runtime regression
    // is suspected to involve the streaming layer specifically.
    streaming: bool,
    // `--json` flips stdout to a single structured envelope at end of
    // dispatch (Phase-A). Default stays human-readable for direct CLI
    // use; JSON is opt-in for consumers like the qa-review skill that
    // need machine-parseable output. All progress/status lines go to
    // stderr when JSON is set so stdout is clean for `jq`.
    json_mode: bool,
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
    allowed_tools: Option<Vec<String>>,
    // (#368 / #482) Compaction config flags — explicit values from the
    // host (which derives them from `profile.runtime.compaction.*` in
    // the typed schema landed in #357). The runtime requires at least
    // one of `--context-window` or `--compact-threshold-tokens` per
    // #482's `validate_compaction_cli_inputs`; the absolute fallback
    // const that pre-#482 covered the all-None case is gone. Env vars
    // are NOT consulted.
    compact_threshold_tokens: Option<u32>,
    compactor_model: Option<String>,
    // (#2808) The window the COMPACTOR loads at, resolved host-side by
    // `resolve_compactor_load_window` and passed through so compaction can
    // size its excerpt to the model that has to READ it. Absent ⇒ unbounded,
    // exactly as before.
    compactor_context_window: Option<u32>,
    // (#368) Formula-based trigger: max_history_share fraction +
    // loaded context_window. Mirrors openclaw's `maxHistoryShare`.
    // Both must be set for the formula trigger to activate; either
    // missing → formula disabled, absolute-threshold-only behavior.
    compact_threshold_ratio: Option<f32>,
    context_window: Option<u32>,
    // (#372 T2-C) Compaction strategy override. When None, runtime
    // uses default Narrative; operator opts into tier-2 by setting
    // `profile.runtime.compaction.strategy: "structured-slot"` which
    // host plumbs via `--compact-strategy structured-slot`.
    compact_strategy: Option<compaction::CompactionStrategy>,
    // (#377) Escalation bound — after this many compactions, the
    // runtime exits with TerminalReason::EscalationTriggered. Host
    // plumbs from `profile.runtime.compaction.reserve.bail_after_compactions`
    // (typed field landed in #357; consumer is #377). None = unbounded.
    bail_after_compactions: Option<u32>,
    // (#383) Operator-tunable custom instructions for the compactor.
    // Host plumbs from `profile.runtime.compaction.custom_instructions`
    // (typed field, schema-isolation doctrine). None = no augmentation.
    compactor_custom_instructions: Option<String>,
    // (#457) Operator-opt-in caps on per-dispatch turn count + cumulative
    // completion tokens. Host derives from `DARKMUX_RUNTIME_MAX_TURNS` /
    // `DARKMUX_RUNTIME_MAX_TOKENS` env vars. Both default unlimited;
    // the inactivity timeout (#458) catches the genuine-stuck case.
    max_turns: Option<u32>,
    max_tokens: Option<u32>,
    // (#1221) Per-call completion-token cap override. Host derives from
    // `DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL`. None = the built-in default
    // (`loop_runner::MAX_TOKENS_PER_CALL` = 10000).
    max_tokens_per_call: Option<u32>,
    reasoning_checkpoint_interval: Option<u32>,
    // (#2171) Host derives from `DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL`.
    // None = the built-in default (`loop_runner::GENERATION_CHECKPOINT_INTERVAL`
    // = 4000).
    generation_checkpoint_interval: Option<u32>,
    // (#2190) The stall-recovery budget override — how many useless
    // turns (empty `tool_calls`, or a runaway-reasoning cut) the
    // runtime tolerates before escalating out of local-tier. Host derives from
    // `DARKMUX_RUNTIME_MAX_STALL_RECOVERIES` / `config.runtime.
    // max_stall_recoveries`. None = the built-in default
    // (`loop_runner::MAX_STALL_RECOVERIES` = 2). No `-source` companion
    // flag — unlike the three per-call knobs above, this budget isn't a
    // `BoundKind` the runtime stamps per-hit provenance for.
    max_stall_recoveries: Option<u32>,
    // (#2165) Provenance for the three knobs above — which of
    // `env(DARKMUX_RUNTIME_*)` / `config.json` / built-in resolved the
    // value the host just forwarded. The host computes this via
    // `darkmux_types::config_access`'s `_with_source` accessors (this crate
    // can't depend on `darkmux-types` — see `runtime/Cargo.toml`'s doc) and
    // passes it as a companion `--<flag>-source` flag. Unrecognized/absent
    // falls to "built-in" via `BoundSource::from_cli_str`.
    max_tokens_per_call_source: Option<String>,
    reasoning_checkpoint_interval_source: Option<String>,
    // (#2171 rebase onto #2165) Same companion-source pattern, for the
    // generation check-in.
    generation_checkpoint_interval_source: Option<String>,
    // (#457 Step 2) Per-role feedback-template overrides. Dispatcher
    // serializes Role.feedback_templates to JSON; runtime parses into
    // a BTreeMap<signal_kind, template_string>. Empty/absent = the
    // FeedbackInjector uses its hardcoded defaults.
    feedback_templates: std::collections::BTreeMap<String, String>,
}

/// Where the brain and the compactor are reached (`--base-url`,
/// `--chat-url`, `--dialect`, `--auth-header-stdin` and the compactor's own
/// endpoint flags); [`build_clients`] turns them into clients.
#[derive(Debug, Default)]
struct EndpointArgs {
    base_url: Option<String>,
    // (#1187) Agentic-remote dispatch: when the "brain" is a remote OpenAI-
    // compatible endpoint (Azure OpenAI, OpenAI, ...) instead of local
    // LMStudio, the host passes the FULL chat-completions URL here (Azure
    // needs a `?api-version=` query string that `base_url` + the client's
    // unconditional `/chat/completions` suffix can't express). When set,
    // this overrides `base_url` for request routing.
    chat_url: Option<String>,
    // (#2902) The endpoint's declared request dialect, when it differs from
    // what `--chat-url` presence implies.
    dialect: Option<lmstudio::Dialect>,
    // (#1187) When true, read the remote endpoint's auth header as JSON
    // (`{"header": "...", "value": "..."}`) from stdin ONCE at startup — the
    // host pipes it in immediately after spawning this container (with `-i`)
    // and closes the pipe. Deliberately NOT a mounted file: `bash` has no
    // `/workspace`-escape check (it's the general-purpose escape hatch), so
    // a secret-bearing file anywhere the container can see is reachable by
    // a model-issued `cat` at any point during the run. Stdin is read
    // exactly once, before the agent loop (and any tool call) exists, and
    // leaves no FILESYSTEM artifact afterward (host or container). This
    // closes the file-based exposure entirely, but is not a complete
    // isolation boundary: the secret still lives in this process's memory
    // for the dispatch's duration, and a same-uid `bash` process could in
    // principle attempt `/proc/<this-process's-pid>/mem` inspection
    // depending on the host's `kernel.yama.ptrace_scope` — a residual risk
    // `--cap-drop=ALL` doesn't close (it blocks `ptrace()`, not a same-uid
    // `/proc/<pid>/mem` read). (#2481) Since `docker run --init` gave the
    // container a real PID 1 (docker's tini, which reaps the orphans #2215's
    // bash-tool fix can leave behind), this process is PID 2, not PID 1 —
    // the attack this note describes is unaffected (same-uid `/proc/<pid>/mem`
    // access doesn't care which pid), but a reader shouldn't infer this
    // process is still the container's init.
    // False when the remote endpoint declares no auth (or no remote brain).
    auth_header_stdin: bool,
    // (#3207) The compactor on an UNMANAGED utility endpoint: its full
    // chat-completions URL, its dialect, and whether its auth header rides
    // the stdin blob (under `compactor`). All absent: the compactor talks to
    // LM Studio at `--base-url`, as before.
    compactor: CompactorEndpointArgs,
}

/// A dispatch refused before it starts (a bad flag, an unreadable
/// checkpoint, a broken auth pipe): the stderr line, and whether it exits
/// through `std::process::exit(2)` rather than returning exit code 2. The
/// exit code is the same; the check-in interval flags have always used the
/// former.
#[derive(Debug, PartialEq)]
struct Refusal {
    message: String,
    process_exit: bool,
}

/// The flags whose refusals exit through `std::process::exit(2)`.
const PROCESS_EXIT_FLAGS: [&str; 4] = [
    "--reasoning-checkpoint-interval",
    "--reasoning-checkpoint-interval-source",
    "--generation-checkpoint-interval",
    "--generation-checkpoint-interval-source",
];

impl Refusal {
    /// `message`, exiting 2 by return.
    fn new(message: String) -> Self {
        Refusal { message, process_exit: false }
    }

    /// `message`, refusing `flag` the way that flag always exits.
    fn refuse(flag: &str, message: String) -> Self {
        Refusal { message, process_exit: PROCESS_EXIT_FLAGS.contains(&flag) }
    }

    /// Print the refusal and exit 2.
    fn exit(self) -> ExitCode {
        eprintln!("{}", self.message);
        if self.process_exit {
            std::process::exit(2);
        }
        ExitCode::from(2)
    }
}

/// The value after `flag`, or the "`flag` requires a value" refusal.
fn required<'a>(flag: &str, value: Option<&'a String>) -> Result<&'a String, Refusal> {
    value.ok_or_else(|| Refusal::refuse(flag, format!("{flag} requires a value")))
}

type TextSlot = fn(&mut RunArgs) -> &mut Option<String>;
type CountSlot = fn(&mut RunArgs) -> &mut Option<u32>;

/// Flags that take any string, stored as given. A later repeat wins.
const TEXT_FLAGS: [(&str, TextSlot); 14] = [
    ("--model", |a| &mut a.model),
    ("--prompt", |a| &mut a.prompt),
    ("--system", |a| &mut a.system),
    ("--response-schema", |a| &mut a.response_schema),
    ("--base-url", |a| &mut a.endpoints.base_url),
    ("--chat-url", |a| &mut a.endpoints.chat_url),
    ("--resume", |a| &mut a.resume_checkpoint_path),
    ("--role-id", |a| &mut a.role_id),
    ("--execution-id", |a| &mut a.execution_id),
    ("--compactor-model", |a| &mut a.compactor_model),
    ("--compactor-custom-instructions", |a| &mut a.compactor_custom_instructions),
    ("--max-tokens-per-call-source", |a| &mut a.max_tokens_per_call_source),
    ("--reasoning-checkpoint-interval-source", |a| &mut a.reasoning_checkpoint_interval_source),
    ("--generation-checkpoint-interval-source", |a| &mut a.generation_checkpoint_interval_source),
];

/// Flags that take a `u32`: the slot, the smallest value accepted, and the
/// noun the refusal names.
const COUNT_FLAGS: [(&str, CountSlot, u32, &str); 10] = [
    ("--compact-threshold-tokens", |a| &mut a.compact_threshold_tokens, 0, "a positive integer"),
    ("--compactor-context-window", |a| &mut a.compactor_context_window, 0, "a positive integer"),
    ("--context-window", |a| &mut a.context_window, 0, "a positive integer"),
    ("--bail-after-compactions", |a| &mut a.bail_after_compactions, 0, "a positive integer"),
    ("--max-turns", |a| &mut a.max_turns, 0, "a positive integer"),
    ("--reasoning-checkpoint-interval", |a| &mut a.reasoning_checkpoint_interval, 1, "a positive integer"),
    ("--generation-checkpoint-interval", |a| &mut a.generation_checkpoint_interval, 1, "a positive integer"),
    ("--max-stall-recoveries", |a| &mut a.max_stall_recoveries, 0, "a non-negative integer"),
    ("--max-tokens-per-call", |a| &mut a.max_tokens_per_call, 0, "a positive integer"),
    ("--max-tokens", |a| &mut a.max_tokens, 0, "a positive integer"),
];

/// Parse `run`'s flags. `resume_env` is `DARKMUX_RESUME_CHECKPOINT`, which
/// `--resume` overrides. Refuses the first bad flag; never checks that the
/// required flags are present ([`RunArgs::take_model_and_prompt`] does).
fn parse_run_args(args: &[String], resume_env: Option<String>) -> Result<RunArgs, Refusal> {
    let mut parsed =
        RunArgs { resume_checkpoint_path: resume_env, streaming: true, ..RunArgs::default() };
    let mut i = 0;
    while i < args.len() {
        i += parsed.apply(&args[i], args.get(i + 1))?;
    }
    Ok(parsed)
}

impl RunArgs {
    /// Apply one flag (and its value): how many arguments it consumed.
    fn apply(&mut self, flag: &str, value: Option<&String>) -> Result<usize, Refusal> {
        if let Some((_, slot)) = TEXT_FLAGS.iter().find(|(f, _)| *f == flag) {
            *slot(self) = Some(required(flag, value)?.clone());
            return Ok(2);
        }
        if let Some((_, slot, min, noun)) = COUNT_FLAGS.iter().find(|(f, ..)| *f == flag) {
            *slot(self) = Some(parse_count(flag, required(flag, value)?, *min, noun)?);
            return Ok(2);
        }
        self.apply_switch(flag).map_or_else(|| self.apply_special(flag, value), Ok)
    }

    /// The flags that take no value.
    fn apply_switch(&mut self, flag: &str) -> Option<usize> {
        match flag {
            "--auth-header-stdin" => self.endpoints.auth_header_stdin = true,
            "--no-stream" => self.streaming = false,
            "--json" => self.json_mode = true,
            _ => return None,
        }
        Some(1)
    }

    /// The flags with their own value rules, then the compactor's endpoint
    /// flags, then the unknown-flag refusal.
    fn apply_special(&mut self, flag: &str, value: Option<&String>) -> Result<usize, Refusal> {
        match flag {
            "--prompt-file" => self.apply_prompt_file(flag, value),
            "--dialect" => self.apply_dialect(flag, value),
            "--allowed-tools" => self.apply_allowed_tools(flag, value),
            "--compact-threshold-ratio" => self.apply_ratio(flag, value),
            "--compact-strategy" => self.apply_compact_strategy(flag, value),
            "--feedback-templates-json" => self.apply_feedback_templates(flag, value),
            other => self.apply_compactor_endpoint(other, value),
        }
    }

    // (#3207) The compactor's own unmanaged endpoint, parsed in one
    // place (`CompactorEndpointArgs::parse`); anything else is unknown.
    fn apply_compactor_endpoint(&mut self, flag: &str, value: Option<&String>) -> Result<usize, Refusal> {
        match self.endpoints.compactor.parse(flag, value) {
            Ok(Some(consumed)) => Ok(consumed),
            Ok(None) => Err(Refusal::refuse(flag, format!("unknown flag: {flag}"))),
            Err(e) => Err(Refusal::refuse(flag, e)),
        }
    }

    fn apply_dialect(&mut self, flag: &str, value: Option<&String>) -> Result<usize, Refusal> {
        let v = required(flag, value)?;
        self.endpoints.dialect = Some(lmstudio::Dialect::parse(v).ok_or_else(|| Refusal::refuse(flag, format!("--dialect must be `chat-completions`, `chat-completions-max-tokens` or `messages` (got {v:?})")))?);
        Ok(2)
    }

    fn apply_ratio(&mut self, flag: &str, value: Option<&String>) -> Result<usize, Refusal> {
        self.compact_threshold_ratio = Some(parse_ratio(flag, required(flag, value)?)?);
        Ok(2)
    }

    fn apply_allowed_tools(&mut self, flag: &str, value: Option<&String>) -> Result<usize, Refusal> {
        let v = value.ok_or_else(|| {
            Refusal::refuse(flag, "--allowed-tools requires a comma-separated list".into())
        })?;
        self.allowed_tools = Some(parse_tool_list(v));
        Ok(2)
    }

    // (#386) Read the user prompt from a mounted file instead of argv,
    // so a substantial brief never lands on the `docker run` command
    // line (where it would hit ARG_MAX and show up in `ps`). The host
    // writes the message into a per-dispatch mount; this reads it back.
    fn apply_prompt_file(&mut self, flag: &str, value: Option<&String>) -> Result<usize, Refusal> {
        if self.prompt.is_some() {
            return Err(Refusal::refuse(
                flag,
                "--prompt and --prompt-file are mutually exclusive".into(),
            ));
        }
        let v = required(flag, value)?;
        let contents = std::fs::read_to_string(v)
            .map_err(|e| Refusal::refuse(flag, format!("--prompt-file {v}: {e}")))?;
        self.prompt = Some(contents);
        Ok(2)
    }

    fn apply_compact_strategy(&mut self, flag: &str, value: Option<&String>) -> Result<usize, Refusal> {
        let v = value.ok_or_else(|| {
            Refusal::refuse(
                flag,
                "--compact-strategy requires a value (`narrative` or `structured-slot`)".into(),
            )
        })?;
        let s = compaction::CompactionStrategy::from_cli_str(v)
            .map_err(|msg| Refusal::refuse(flag, format!("--compact-strategy: {msg}")))?;
        self.compact_strategy = Some(s);
        Ok(2)
    }

    /// Bad JSON warns and keeps whatever was set before (the defaults,
    /// unless an earlier `--feedback-templates-json` parsed).
    fn apply_feedback_templates(&mut self, flag: &str, value: Option<&String>) -> Result<usize, Refusal> {
        let v = value.ok_or_else(|| {
            Refusal::refuse(flag, "--feedback-templates-json requires a JSON-string value".into())
        })?;
        match serde_json::from_str::<std::collections::BTreeMap<String, String>>(v) {
            Ok(map) => self.feedback_templates = map,
            Err(e) => eprintln!(
                "--feedback-templates-json failed to parse: {e}. \
                 Expected JSON object of {{signal_kind: template_string}}. \
                 Ignoring; runtime will use defaults. (#457 Step 2)"
            ),
        }
        Ok(2)
    }
}

/// A `u32` flag's value, at least `min`, or the refusal naming `noun`.
fn parse_count(flag: &str, v: &str, min: u32, noun: &str) -> Result<u32, Refusal> {
    v.parse::<u32>()
        .ok()
        .filter(|n| *n >= min)
        .ok_or_else(|| Refusal::refuse(flag, format!("{flag} requires {noun} (got: {v})")))
}

/// `--model` and `--prompt`, which every dispatch needs; `--model` is
/// checked first.
fn required_model_and_prompt(
    model: Option<String>,
    prompt: Option<String>,
) -> Result<(String, String), Refusal> {
    let model = model.ok_or_else(|| {
        Refusal::refuse(
            "--model",
            "--model is required (e.g. darkmux:qwen3.6-35b-a3b-turboquant-mlx)".into(),
        )
    })?;
    let prompt =
        prompt.ok_or_else(|| Refusal::refuse("--prompt", "--prompt is required".into()))?;
    Ok((model, prompt))
}

/// The checkpoint `--resume`/`DARKMUX_RESUME_CHECKPOINT` named, if any. A
/// missing/corrupt checkpoint is refused rather than silently starting
/// fresh under a name that looked like a resume.
fn load_resume_checkpoint(path: Option<&str>) -> Result<Option<checkpoint::RunCheckpoint>, Refusal> {
    path.map(|path| {
        checkpoint::read_checkpoint(Path::new(path))
            .map_err(|e| Refusal::new(format!("--resume {path}: {e:#}")))
    })
    .transpose()
}

/// `--compact-threshold-ratio`: a fraction in 0.1-0.9.
fn parse_ratio(flag: &str, v: &str) -> Result<f32, Refusal> {
    let f = v.parse::<f32>().map_err(|_| {
        Refusal::refuse(flag, format!("--compact-threshold-ratio requires a fraction in 0.1-0.9 (got: {v})"))
    })?;
    if !(0.1..=0.9).contains(&f) {
        return Err(Refusal::refuse(
            flag,
            format!("--compact-threshold-ratio must be in range 0.1-0.9 (got: {f})"),
        ));
    }
    Ok(f)
}

/// `--allowed-tools`: the comma-separated names, trimmed, blanks dropped.
fn parse_tool_list(v: &str) -> Vec<String> {
    v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// Construct the `--json` envelope. Pure function — extracted so the
/// schema is tested independently of the dispatch shell-out (which
/// requires LMStudio + Docker). Same shape for success + error paths so
/// consumers parse uniformly.
///
/// `final_assistant = None` produces a JSON `null` for the field — the
/// error envelope uses this; a success envelope always carries a string.
///
/// Carries no counts. Turns, tokens and rests are in the trajectory the
/// envelope points at, written as they happened; the host folds that one
/// log (`darkmux_trajectory::TrajectoryFold`) and adds the `metrics` block
/// its callers read, so no second tally exists to disagree with it.
fn build_json_envelope(result: &str, final_assistant: Option<&str>) -> darkmux_trajectory::RuntimeEnvelope {
    darkmux_trajectory::RuntimeEnvelope {
        result: Some(result.to_string()),
        final_assistant: final_assistant.map(str::to_string),
        // Container-internal path where the runtime's own bookkeeping
        // landed — the out-dir (SEPARATE from /workspace). Built from the
        // shared trajectory constants so it can't drift from the write site.
        trajectory_path: Some(
            darkmux_trajectory::trajectory_path(&trajectory::out_base()).display().to_string(),
        ),
        failed_tool_invocations: None,
        resumed_from: None,
    }
}

/// Human mode's closing summary: this execution's counts, read back from
/// its own trajectory through the one fold the host uses.
fn print_run_summary(result: &str, wall_ms: u128) {
    let fold = darkmux_trajectory::TrajectoryFold::from_path(&darkmux_trajectory::trajectory_path(
        &trajectory::out_base(),
    ));
    println!("--- run ---");
    println!("result:            {result}");
    println!("turns:             {}", fold.turns());
    println!("compactions:       {}", fold.compactions());
    println!("prompt tokens:     {}", fold.tokens.prompt);
    println!("completion tokens: {}", fold.tokens.completion);
    // (#1444) Only printed when at least one call reported it: "not shown"
    // reads as "unknown", never a fabricated 0. No "(subset of completion
    // tokens)" gloss: whether reasoning sits inside completion is
    // provider-specific, and this line does not know the provider.
    if let Some(rt) = fold.tokens.reasoning {
        println!("reasoning tokens:  {rt}");
    }
    if let Some(ct) = fold.tokens.cached {
        println!("cached tokens:     {ct}");
    }
    println!("rest:              {}ms over {} rest(s)", fold.rest_ms(), fold.rest_count());
    println!("wall:              {wall_ms}ms");
}

/// (#1187) Read the auth-header JSON from stdin ONCE and parse it. Reads to
/// EOF — the host writes exactly one JSON blob and closes the pipe
/// immediately after spawning this container, so `read_to_string` returns
/// as soon as that write completes. Nothing is ever written to any
/// filesystem: unlike a mounted file, there is no FILESYSTEM artifact for a
/// `bash`-capable model to find at any point during the run — `bash` has no
/// `/workspace`-escape check (it's the deliberate general-purpose escape
/// hatch), so any secret-bearing FILE the container can see would be
/// reachable via a plain `cat`. Stdin has no such window (this doesn't
/// close every exposure vector — see the residual-risk note on
/// `auth_header_stdin`'s declaration above). Returns `(header_name,
/// header_value)` on success.
/// (#1187, #3207) A model client: LM Studio at `base_url` (the runtime's
/// default when absent), or the full `chat_url` of an endpoint darkmux does
/// not manage, in its `dialect` and with its `auth` header. The brain's and
/// the compactor's are built here from their own flags: the compactor never
/// gets the brain's `--chat-url` or auth.
fn build_client(
    base_url: Option<&str>,
    chat_url: Option<String>,
    dialect: Option<lmstudio::Dialect>,
    auth: Option<(String, String)>,
) -> LmStudioClient {
    let mut client = match base_url {
        Some(url) => LmStudioClient::with_base_url(url),
        None => LmStudioClient::new(),
    };
    if let Some(url) = chat_url {
        client = client.with_chat_url(url);
    }
    if let Some(d) = dialect {
        client = client.with_dialect(d);
    }
    if let Some((h, v)) = auth {
        client = client.with_auth_header(h, v);
    }
    client
}

/// The brain's client and the compactor's, in that order, after reading the
/// stdin auth blob (a read or parse failure is refused).
fn build_clients(endpoints: EndpointArgs) -> Result<(LmStudioClient, LmStudioClient), Refusal> {
    let EndpointArgs { base_url, chat_url, dialect, auth_header_stdin, compactor } = endpoints;
    let stdin_auth =
        read_stdin_auth(auth_header_stdin, compactor.auth_stdin).map_err(Refusal::new)?;
    let compactor_client = build_client(
        base_url.as_deref(),
        compactor.chat_url,
        compactor.dialect,
        stdin_auth.compactor,
    );
    let client = build_client(base_url.as_deref(), chat_url, dialect, stdin_auth.brain);
    Ok((client, compactor_client))
}

/// (#3207) The compactor's own unmanaged endpoint, from
/// `--compactor-chat-url`, `--compactor-dialect` and
/// `--compactor-auth-header-stdin`. All absent: the compactor talks to LM
/// Studio at `--base-url`, as before.
#[derive(Debug, Default, PartialEq)]
struct CompactorEndpointArgs {
    chat_url: Option<String>,
    dialect: Option<lmstudio::Dialect>,
    auth_stdin: bool,
}

impl CompactorEndpointArgs {
    /// Parse `flag` (and its `value`): `Ok(Some(n))` consumed `n` arguments,
    /// `Ok(None)` the flag is not one of these, `Err` the message to exit 2 on.
    fn parse(&mut self, flag: &str, value: Option<&String>) -> Result<Option<usize>, String> {
        let needs_value = || value.cloned().ok_or_else(|| format!("{flag} requires a value"));
        match flag {
            "--compactor-chat-url" => self.chat_url = Some(needs_value()?),
            "--compactor-dialect" => {
                let v = needs_value()?;
                self.dialect = Some(lmstudio::Dialect::parse(&v).ok_or_else(|| {
                    format!("{flag} must be `chat-completions`, `chat-completions-max-tokens` or `messages` (got {v:?})")
                })?);
            }
            "--compactor-auth-header-stdin" => {
                self.auth_stdin = true;
                return Ok(Some(1));
            }
            _ => return Ok(None),
        }
        Ok(Some(2))
    }
}

/// (#1187, #3207) The auth headers piped over stdin: the brain's
/// (`--auth-header-stdin`) and the compactor's
/// (`--compactor-auth-header-stdin`), each `None` unless its flag is set.
#[derive(Debug, Default, PartialEq)]
struct StdinAuth {
    brain: Option<(String, String)>,
    compactor: Option<(String, String)>,
}

/// Read stdin ONCE, and only when a flag asks for a header on it.
fn read_stdin_auth(brain: bool, compactor: bool) -> Result<StdinAuth, String> {
    if !(brain || compactor) {
        return Ok(StdinAuth::default());
    }
    let mut contents = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut contents)
        .map_err(|e| format!("--auth-header-stdin: {e}"))?;
    stdin_auth_from(&contents, brain, compactor)
}

/// The pure half of [`read_stdin_auth`]: each header the flags ask for,
/// parsed from the one blob, the error naming the flag.
fn stdin_auth_from(contents: &str, brain: bool, compactor: bool) -> Result<StdinAuth, String> {
    Ok(StdinAuth {
        brain: brain
            .then(|| parse_auth_header_json(contents).map_err(|e| format!("--auth-header-stdin: {e}")))
            .transpose()?,
        compactor: compactor
            .then(|| parse_compactor_auth_header_json(contents).map_err(|e| format!("--compactor-auth-header-stdin: {e}")))
            .transpose()?,
    })
}

/// (#3207) The compactor's header from the same stdin blob, under
/// `{"compactor": {"header": "...", "value": "..."}}`.
fn parse_compactor_auth_header_json(contents: &str) -> Result<(String, String), String> {
    let v: serde_json::Value =
        serde_json::from_str(contents).map_err(|e| format!("invalid JSON: {e}"))?;
    let c = v.get("compactor");
    let header = c.and_then(|c| c.get("header")).and_then(|h| h.as_str());
    let value = c.and_then(|c| c.get("value")).and_then(|h| h.as_str());
    match (header, value) {
        (Some(h), Some(val)) => Ok((h.to_string(), val.to_string())),
        _ => Err("expected {\"compactor\": {\"header\": \"...\", \"value\": \"...\"}}".to_string()),
    }
}

/// The brain's header from the stdin blob ([`read_stdin_auth`]), split out
/// so the parsing/validation logic is unit-testable without touching the
/// process's real stdin.
fn parse_auth_header_json(contents: &str) -> Result<(String, String), String> {
    let v: serde_json::Value =
        serde_json::from_str(contents).map_err(|e| format!("invalid JSON: {e}"))?;
    let header = v.get("header").and_then(|h| h.as_str());
    let value = v.get("value").and_then(|h| h.as_str());
    match (header, value) {
        (Some(h), Some(val)) => Ok((h.to_string(), val.to_string())),
        _ => Err("expected {\"header\": \"...\", \"value\": \"...\"}".to_string()),
    }
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
/// (#2268) The tools this run ADVERTISES to the model. `full_catalog` is the
/// general-purpose set every role can draw from; an explicit `--allowed-tools`
/// list is authoritative and may name a tool that is NOT in the catalog
/// (`create_finding` — crawler-only, deliberately kept out of the general
/// catalog so a role with an empty palette never sees it). Before this, such
/// a name was filtered against the catalog and vanished: the host passed
/// `--allowed-tools read,search,bash,create_finding`, the runtime advertised
/// `search,read,bash`, and since #2182 derived the execution allow-list from
/// the ADVERTISED set, so the crawler could neither see nor call the one
/// tool its role exists for. It had only ever worked because the pre-#2182
/// structured path executed any `from_name`-resolvable call unadvertised.
///
/// Rule: catalog ∩ allow (catalog order, as before), then every allowed name
/// the catalog lacks that `Tool::from_name` resolves (allow order), deduped.
/// An allowed name nothing resolves is reported on stderr and ignored — the
/// host validates role vocabulary, so this is a host/runtime version skew
/// signal. Note the host keeps a successful unit's stderr only as a byte
/// count (`stderr_excerpt` is `None` on exit 0), so on the crawl path the
/// durable signal is the artifact instead: `tools_requested` on the host's
/// `dispatch start` record versus `tools` on the trajectory's.
/// `None` (no list) is the full catalog, unchanged.
fn advertised_tools(full_catalog: &[Tool], allowed: Option<&[String]>) -> Vec<Tool> {
    let mut tools = filter_tools_by_allowed(full_catalog, allowed);
    if let Some(names) = allowed {
        for name in names {
            if tools.iter().any(|t| t.name() == name) {
                continue;
            }
            match Tool::from_name(name) {
                Some(t) => tools.push(t),
                None => eprintln!(
                    "darkmux-runtime: --allowed-tools names `{name}`, which is not a tool this \
                     runtime knows; ignoring it (known: {})",
                    Tool::ALL.iter().map(|t| t.name()).collect::<Vec<_>>().join(", ")
                ),
            }
        }
    }
    tools
}

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

    // (#2268) RED before the fix: `create_finding` is a real tool with a
    // `from_name` mapping, is named by the crawler role's palette, and was
    // never advertised because it is not in `full_catalog`.
    #[test]
    fn every_named_tool_is_advertised_when_the_allow_list_names_it() {
        let full = [Tool::Search, Tool::Read, Tool::Edit, Tool::Write, Tool::Bash];
        for t in Tool::ALL {
            let allow = vec![t.name().to_string()];
            let out = advertised_tools(&full, Some(&allow));
            assert!(
                out.iter().any(|x| x.name() == t.name()),
                "`{}` is named by --allowed-tools but was not advertised: {:?}",
                t.name(),
                out.iter().map(|x| x.name()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn advertised_tools_keeps_catalog_order_then_appends_extras_in_allow_order() {
        let full = [Tool::Search, Tool::Read, Tool::Edit, Tool::Write, Tool::Bash];
        // the crawler's palette as the host actually emits it (`read` → read,search;
        // `exec` → bash; then create_finding): catalog members come out in CATALOG
        // order regardless of allow order, then the extra — so create_finding is LAST.
        let allow: Vec<String> = ["read", "search", "bash", "create_finding"].iter().map(|s| s.to_string()).collect();
        let names: Vec<&str> = advertised_tools(&full, Some(&allow)).iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["search", "read", "bash", "create_finding"]);
        // and allow order does not change that
        let allow2: Vec<String> = ["create_finding", "bash", "read", "search"].iter().map(|s| s.to_string()).collect();
        let names2: Vec<&str> = advertised_tools(&full, Some(&allow2)).iter().map(|t| t.name()).collect();
        assert_eq!(names2, names);
    }

    #[test]
    fn advertised_tools_none_is_the_full_catalog_and_never_adds_create_finding() {
        let full = [Tool::Search, Tool::Read, Tool::Edit, Tool::Write, Tool::Bash];
        let out = advertised_tools(&full, None);
        assert_eq!(out.len(), 5);
        assert!(!out.iter().any(|t| matches!(t, Tool::CreateFinding)), "an empty palette must not expose create_finding");
    }

    #[test]
    fn advertised_tools_ignores_an_unknown_name_and_dedupes() {
        let full = [Tool::Search, Tool::Read, Tool::Edit, Tool::Write, Tool::Bash];
        let allow: Vec<String> = ["read", "not_a_tool", "read", "create_finding", "create_finding"].iter().map(|s| s.to_string()).collect();
        let names: Vec<&str> = advertised_tools(&full, Some(&allow)).iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["read", "create_finding"]);
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

    // ─── parse_auth_header_json (#1187 stdin auth) ───────────────────

    /// (#3207) The compactor's header rides the same stdin blob, under
    /// `compactor`, beside (or without) the brain's.
    #[test]
    fn the_compactor_header_is_read_from_its_own_key_in_the_stdin_blob() {
        let both = r#"{"header":"api-key","value":"brain","compactor":{"header":"Authorization","value":"Bearer util"}}"#;
        assert_eq!(parse_auth_header_json(both).unwrap(), ("api-key".to_string(), "brain".to_string()));
        assert_eq!(
            parse_compactor_auth_header_json(both).unwrap(),
            ("Authorization".to_string(), "Bearer util".to_string())
        );
        let only = r#"{"compactor":{"header":"api-key","value":"util"}}"#;
        assert_eq!(parse_compactor_auth_header_json(only).unwrap(), ("api-key".to_string(), "util".to_string()));
        assert!(parse_auth_header_json(only).is_err(), "no brain header in a compactor-only blob");
        assert!(parse_compactor_auth_header_json(r#"{"header":"h","value":"v"}"#).is_err());
    }

    /// (#3207) The compactor's flags parse into their own struct; anything
    /// else is not theirs; a bad dialect or a missing value is refused.
    #[test]
    fn the_compactor_endpoint_flags_parse_on_their_own() {
        let mut c = CompactorEndpointArgs::default();
        let url = "http://host.docker.internal:8080/v1/chat/completions".to_string();
        assert_eq!(c.parse("--compactor-chat-url", Some(&url)), Ok(Some(2)));
        assert_eq!(c.parse("--compactor-dialect", Some(&"chat-completions-max-tokens".to_string())), Ok(Some(2)));
        assert_eq!(c.parse("--compactor-auth-header-stdin", None), Ok(Some(1)));
        assert_eq!(
            c,
            CompactorEndpointArgs {
                chat_url: Some(url),
                dialect: Some(lmstudio::Dialect::ChatCompletionsMaxTokens),
                auth_stdin: true
            }
        );
        assert_eq!(c.parse("--chat-url", Some(&"x".to_string())), Ok(None), "the brain's flag is not the compactor's");
        assert!(c.parse("--compactor-dialect", Some(&"nope".to_string())).unwrap_err().contains("must be"));
        assert!(c.parse("--compactor-chat-url", None).unwrap_err().contains("requires a value"));
        assert_eq!(stdin_auth_from("", false, false), Ok(StdinAuth::default()));
        let both = r#"{"header":"api-key","value":"b","compactor":{"header":"api-key","value":"c"}}"#;
        let auth = stdin_auth_from(both, true, true).unwrap();
        assert_eq!((auth.brain.unwrap().1, auth.compactor.unwrap().1), ("b".to_string(), "c".to_string()));
        assert!(stdin_auth_from(r#"{"header":"h","value":"v"}"#, true, true).unwrap_err().starts_with("--compactor-auth-header-stdin"));
    }

    /// (#3207) A utility model on an UNMANAGED endpoint: the compaction call
    /// goes to that endpoint's chat URL (never LM Studio at `base_url`), with
    /// the bare compactor id, the endpoint's dialect (`max_tokens`) and its
    /// auth header.
    #[test]
    #[serial_test::serial]
    fn a_compactor_on_an_unmanaged_endpoint_is_sent_there_with_its_dialect_and_auth() {
        use httpmock::prelude::*;
        let lm_studio = crate::test_support::GuardedMockServer::start();
        let never = lm_studio.mock_expect_zero("the compactor must not call LM Studio when it has its own endpoint", |when, then| {
            when.method(POST);
            then.status(500);
        });
        let endpoint = crate::test_support::GuardedMockServer::start();
        let summary = "Summary: the assistant read the workspace file several times and inspected \
                       what it returned. Nothing was decided and nothing was modified yet. The next \
                       step is to keep reading the file and then act on what it contains.";
        let hit = endpoint.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .header("authorization", "Bearer util-secret")
                .json_body_partial(r#"{"model":"mlx-util"}"#)
                .body_contains("\"max_tokens\"");
            then.status(200).json_body(serde_json::json!({
                "id": "c", "object": "chat.completion", "created": 0, "model": "mlx-util",
                "choices": [{ "index": 0, "message": { "role": "assistant", "content": summary }, "finish_reason": "stop" }],
                "usage": { "prompt_tokens": 900, "completion_tokens": 40, "total_tokens": 940 },
            }));
        });
        let client = build_client(
            Some(&format!("{}/v1", lm_studio.base_url())),
            Some(format!("{}/v1/chat/completions", endpoint.base_url())),
            Some(lmstudio::Dialect::ChatCompletionsMaxTokens),
            Some(("Authorization".to_string(), "Bearer util-secret".to_string())),
        );
        let pad = "context detail that occupies transcript space ".repeat(12);
        let mut messages = vec![Message::system("you are an agent"), Message::user("do the thing")];
        for i in 0..4 {
            messages.push(Message::user(format!("user {i}: {pad}")));
            messages.push(Message::assistant(format!("assistant {i}: {pad}")));
        }
        let cfg = compaction::CompactionConfig {
            compactor_model: Some("mlx-util".to_string()),
            ..compaction::CompactionConfig::never_compact()
        };
        compaction::compact(&client, &mut messages, 1, &cfg, &mut Vec::new())
            .expect("the compaction call reaches the unmanaged endpoint");
        hit.assert();
        never.assert_hits(0);
    }

    #[test]
    fn auth_header_json_happy_path_returns_header_and_value() {
        let (h, v) = parse_auth_header_json(r#"{"header": "api-key", "value": "super-secret"}"#).unwrap();
        assert_eq!(h, "api-key");
        assert_eq!(v, "super-secret");
    }

    #[test]
    fn auth_header_json_rejects_malformed_json() {
        let err = parse_auth_header_json("not json at all").unwrap_err();
        assert!(err.contains("invalid JSON"), "got: {err}");
    }

    #[test]
    fn auth_header_json_rejects_missing_keys() {
        let err = parse_auth_header_json(r#"{"wrong_key": "value"}"#).unwrap_err();
        assert!(err.contains("header") && err.contains("value"), "got: {err}");
    }

    #[test]
    fn json_envelope_success_carries_final_assistant_and_no_counts() {
        let env = serde_json::to_value(build_json_envelope("stop", Some("hello world"))).unwrap();
        // Top-level contract — qa-review + lab adapter parse these.
        assert_eq!(env["result"], "stop");
        assert_eq!(env["final_assistant"], "hello world");
        assert_eq!(env["trajectory_path"], "/darkmux-out/.darkmux-runtime/trajectory.jsonl");
        // The counts live in the trajectory, which the host folds into the
        // `metrics` block it hands its callers. A second tally here is the
        // metrics.json this replaced.
        assert!(env.get("metrics").is_none(), "{env}");
    }

    #[test]
    fn json_envelope_error_carries_null_final_assistant() {
        // Failure path emits same envelope shape so consumers can parse
        // uniformly without branching on success/error.
        let env = serde_json::to_value(build_json_envelope("error", None)).unwrap();
        assert_eq!(env["result"], "error");
        assert!(env["final_assistant"].is_null(), "error envelope must have null final_assistant");
        assert!(env.get("metrics").is_none(), "{env}");
    }

    #[test]
    fn json_envelope_serializes_as_single_line() {
        // qa-review parses with `jq -c` — verify the serialized form is
        // single-line + valid JSON. (No surprise whitespace, etc.)
        let env = serde_json::to_value(build_json_envelope("stop", Some("x"))).unwrap();
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
        let env = serde_json::to_value(build_json_envelope("stop", Some(tricky))).unwrap();
        let s = serde_json::to_string(&env).unwrap();
        let back: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back["final_assistant"], tricky);
    }
}
