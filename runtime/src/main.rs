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
mod lmstudio;
mod loop_runner;
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
            println!("  darkmux-runtime run --model <id> --system <text> --prompt <text> [--no-stream]");
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
    let tools = [Tool::Search, Tool::Read, Tool::Edit, Tool::Write, Tool::Bash];

    println!("dispatching to model: {model}");
    println!();

    // Open trajectory + metrics recorder against the mounted
    // workspace. Phase 7: gives post-dispatch visibility because the
    // container is --rm and otherwise everything except stderr is lost.
    let mut traj = trajectory::Trajectory::open(Path::new("/workspace"));
    let started_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let system_chars = initial_messages[0].content.as_deref().map(str::len).unwrap_or(0);
    let prompt_chars = initial_messages[1].content.as_deref().map(str::len).unwrap_or(0);
    traj.append_dispatch_start(&model, system_chars, prompt_chars);

    let run_result = loop_runner::run(
        &client,
        &model,
        initial_messages,
        &tools,
        &mut traj,
        streaming,
    );

    let (outcome, success) = match run_result {
        Ok(o) => (Some(o), true),
        Err(e) => {
            eprintln!("dispatch failed: {e:#}");
            (None, false)
        }
    };

    // Whether success or failure, write the trajectory close + metrics.
    let wall_ms = traj.elapsed_ms();
    let result_str = if success { "stop" } else { "error" };
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
            max_turns_reached: false,
            final_assistant_preview: preview,
        };
        let _ = traj.save_metrics(&metrics);

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
        return ExitCode::from(1);
    }

    ExitCode::SUCCESS
}
