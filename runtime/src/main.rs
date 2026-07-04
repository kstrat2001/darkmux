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
    // (#1038) Raw JSON-schema string for the role's output, wrapped into an
    // LMStudio json_schema response_format before the loop. None ⇒ free-form.
    let mut response_schema: Option<String> = None;
    let mut base_url: Option<String> = None;
    // (#1187) Agentic-remote dispatch: when the "brain" is a remote OpenAI-
    // compatible endpoint (Azure OpenAI, OpenAI, ...) instead of local
    // LMStudio, the host passes the FULL chat-completions URL here (Azure
    // needs a `?api-version=` query string that `base_url` + the client's
    // unconditional `/chat/completions` suffix can't express). When set,
    // this overrides `base_url` for request routing.
    let mut chat_url: Option<String> = None;
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
    // principle attempt `/proc/1/mem` inspection depending on the host's
    // `kernel.yama.ptrace_scope` — a residual risk `--cap-drop=ALL` doesn't
    // close (it blocks `ptrace()`, not a same-uid `/proc/<pid>/mem` read).
    // False when the remote endpoint declares no auth (or no remote brain).
    let mut auth_header_stdin: bool = false;
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
            // (#386) Read the user prompt from a mounted file instead of argv,
            // so a substantial brief never lands on the `docker run` command
            // line (where it would hit ARG_MAX and show up in `ps`). The host
            // writes the message into a per-dispatch mount; this reads it back.
            "--prompt-file" => {
                if prompt.is_some() {
                    eprintln!("--prompt and --prompt-file are mutually exclusive");
                    return ExitCode::from(2);
                }
                if let Some(v) = args.get(i + 1) {
                    match std::fs::read_to_string(v) {
                        Ok(contents) => prompt = Some(contents),
                        Err(e) => {
                            eprintln!("--prompt-file {v}: {e}");
                            return ExitCode::from(2);
                        }
                    }
                    i += 2;
                } else {
                    eprintln!("--prompt-file requires a value");
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
            "--response-schema" => {
                if let Some(v) = args.get(i + 1) {
                    response_schema = Some(v.clone());
                    i += 2;
                } else {
                    eprintln!("--response-schema requires a value");
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
            "--chat-url" => {
                if let Some(v) = args.get(i + 1) {
                    chat_url = Some(v.clone());
                    i += 2;
                } else {
                    eprintln!("--chat-url requires a value");
                    return ExitCode::from(2);
                }
            }
            "--auth-header-stdin" => {
                auth_header_stdin = true;
                i += 1;
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

    // (#1187 audit finding) Build the compactor's client from `base_url`
    // BEFORE it's consumed below, and NEVER apply `--chat-url`/
    // `--auth-header-stdin` to it — the compactor always talks to local
    // LMStudio, even on a remote-brain dispatch. See the doc comment on
    // `loop_runner::run`'s `compactor_client` parameter for why routing
    // the compactor through a remote-configured client is wrong, not just
    // different (it silently burns the wrong model's budget on Azure, or
    // 404s and fails the whole dispatch on OpenAI-style endpoints).
    let compactor_client = match base_url.as_deref() {
        Some(url) => LmStudioClient::with_base_url(url),
        None => LmStudioClient::new(),
    };

    let mut client = match base_url {
        Some(url) => LmStudioClient::with_base_url(url),
        None => LmStudioClient::new(),
    };
    if let Some(url) = chat_url {
        client = client.with_chat_url(url);
    }
    // (#1187) Read the auth header from stdin ONCE at startup — never a CLI
    // arg (ps-visible), never an env var (visible to every process, no
    // permission gate), never a file (bash has no `/workspace`-escape check,
    // so any secret-bearing file the container can see is reachable by a
    // model-issued `cat` at any point during the run — stdin leaves no such
    // FILESYSTEM artifact, since nothing is ever written to disk; see the
    // longer note on `auth_header_stdin`'s declaration for the residual
    // in-memory exposure this does NOT close). A parse/read failure is
    // fatal (exit 2): a remote-brain dispatch with a broken auth pipe would
    // otherwise proceed unauthenticated and fail confusingly on the FIRST
    // model call instead of loudly at startup.
    if auth_header_stdin {
        match read_auth_header_from_stdin() {
            Ok((h, val)) => {
                client = client.with_auth_header(h, val);
            }
            Err(e) => {
                eprintln!("--auth-header-stdin: {e}");
                return ExitCode::from(2);
            }
        }
    }
    let client = client;

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
    let run_result = loop_runner::run(
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
        feedback_templates,
        response_format,
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
            let mut envelope = build_json_envelope(
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
            // (#799) Stamp the verifier-fabrication backstop: the bash commands
            // that FAILED TO RUN this dispatch. The gate cross-checks a SIGNOFF's
            // verification claims against this; empty on an honest run.
            if let Some(obj) = envelope.as_object_mut() {
                obj.insert(
                    "failed_tool_invocations".into(),
                    serde_json::to_value(&o.failed_to_run)
                        .unwrap_or_else(|_| serde_json::json!([])),
                );
            }
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
            // (#884) An error returned from the loop is an infrastructure
            // failure, NOT a turn-cap termination. Hardcoding `true` here
            // mislabeled every infra failure as max-turns and corrupted
            // the #325 three-way result discrimination that downstream
            // consumers branch on.
            max_turns_reached: false,
            final_assistant_preview: String::new(),
        };
        let _ = traj.save_metrics(&metrics);

        if json_mode {
            let envelope = build_json_envelope(
                "error", None, &model, started_at_unix_ms, wall_ms,
                0, 0, 0, 0, 0,
            );
            println!("{}", serde_json::to_string(&envelope).unwrap_or_else(|_| "{}".into()));
        } else {
            // (#905) Human mode was silent on failure while the success path
            // is verbose — print a failure summary so a non-JSON dispatch
            // doesn't just vanish with a bare exit code.
            println!("--- dispatch failed ---");
            println!("result: {result_str}");
            println!("model:  {model}");
            println!("wall:   {wall_ms}ms");
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
fn read_auth_header_from_stdin() -> Result<(String, String), String> {
    use std::io::Read;
    let mut contents = String::new();
    std::io::stdin()
        .read_to_string(&mut contents)
        .map_err(|e| e.to_string())?;
    parse_auth_header_json(&contents)
}

/// Pure JSON-validation half of [`read_auth_header_from_stdin`], split out
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

    // ─── parse_auth_header_json (#1187 stdin auth) ───────────────────

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
