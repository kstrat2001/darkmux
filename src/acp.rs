//! `darkmux acp` — SPIKE (#1388): expose darkmux as an ACP (Agent Client
//! Protocol) agent over stdio so editors like Zed can drive a
//! `darkmux mission launch review` from their agent panel.
//!
//! ## This is a spike, not a shipped feature
//!
//! Optimized for "it works in Zed today", not architectural purity. Things
//! that are deliberately spike-grade here (a real feature would do these
//! differently):
//!
//! - (#1684, Packet 1 — RESOLVED) Commands used to be limited to a single
//!   HARDCODED `/review` with a fixed "not supported" reply for anything
//!   else. `session/new` now advertises every mission config in the merged
//!   registry (built-ins + `~/.darkmux/mission-configs/`) that declares a
//!   `panel` block — see `src/acp_panel.rs`, which owns the registry
//!   enumeration, the ephemeral-vs-mission-launch routing decision, and the
//!   in-process ephemeral graph runner. `review` itself now reaches this
//!   file's bespoke [`run_review`] through that SAME routing path (rather
//!   than a hand-rolled string match), unchanged otherwise.
//! - Review-stage progress ("bundle", "probe", ...) is recognized by
//!   pattern-matching known substrings out of the review subprocess's
//!   stderr (see [`REVIEW_STAGES`] and [`recognize_stage`]). This is
//!   fragile — a wording change in the review pipeline's liveness markers
//!   silently stops advancing the on-screen plan — but it is genuinely the
//!   only signal available without teaching the review pipeline a
//!   structured progress channel, which is out of scope for a spike whose
//!   job is "does ACP+Zed work at all".
//! - Session state (the cwd per ACP session) lives in an in-memory map that
//!   is never pruned. A long-lived `darkmux acp` process leaks one entry
//!   per `session/new` — fine for a spike where the process is one Zed tab.
//! - No cancellation support. `session/cancel` is unhandled, and the
//!   `session/prompt` handler awaits the whole review subprocess in place
//!   (blocking the connection's event loop for the duration — see
//!   `ConnectionTo::spawn`'s docs on why that's normally avoided). A real
//!   implementation would `cx.spawn` the review and wire up
//!   `RequestCancellation` so the user's Zed "stop" button actually stops
//!   the subprocess.
//! - The `case_id` passed to the review mission is derived from the diff's
//!   content hash + the cwd's directory name (see [`derive_case_id`]) —
//!   deterministic (no `Date`/random per the task brief) but not
//!   collision-proof across very different diffs that happen to hash the
//!   same 8 hex chars (astronomically unlikely; not worth guarding for a
//!   spike).
//! - Bundler routing is extension-sniffing on the diff ([`choose_bundler`]):
//!   TypeScript present → the built-in bundler; else `.edge` present → the
//!   operator's `darkmux-bundler-edge` plugin (absolute-path resolved from
//!   `~/.local/bin` because Zed's GUI env may not carry it on PATH). A
//!   mixed ts+edge diff takes the TS path and skips the templates; real
//!   bundler composition/config is follow-up work, not spike work.
//!
//! ## Why stdout is off-limits
//!
//! ACP's wire transport IS stdout: `AcpStdio::new()` below wires the
//! JSON-RPC connection directly to this process's stdin/stdout
//! (`agent_client_protocol::Stdio`, aliased here to avoid colliding with
//! `std::process::Stdio`). `darkmux mission launch review` prints its
//! rendered result to STDOUT (`pr_review::emit_rendered`'s `println!`), so
//! this file NEVER calls the review pipeline in-process — it always shells
//! out to a SUBPROCESS (the current executable, re-invoked as
//! `mission launch review`) with stdout/stderr captured as pipes, and never
//! writes anything but the ACP JSON-RPC stream to this process's own
//! stdout. Anything this file wants to log for its own debugging goes to
//! STDERR only (`[darkmux-acp]`-prefixed, matching the existing
//! `[darkmux-liveness]` convention) — Zed surfaces an agent's stderr in its
//! logs panel.

use agent_client_protocol::schema::v1::{
    AgentCapabilities, AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, ContentBlock,
    ContentChunk, InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, Plan,
    PlanEntry, PlanEntryPriority, PlanEntryStatus, PromptRequest, PromptResponse, SessionId,
    SessionNotification, SessionUpdate, StopReason, ToolCall, ToolCallId, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind, UnstructuredCommandInput,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Stdio as AcpStdio};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio as ProcStdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// The review pipeline's stages in order, paired with a lowercase substring
/// to look for in the subprocess's stderr lines. SPIKE-GRADE recognition
/// (see module docs) — these stems come from reading
/// `crates/darkmux-types/src/dispatch_liveness.rs`'s `[darkmux-liveness]`
/// phase names (`bundling-start`/`bundling-done`, etc.) plus the stage
/// names named in issue #1388 itself. Index into this array doubles as the
/// index into the [`Plan`] entries this file sends, so keep the two in
/// lockstep.
const REVIEW_STAGES: &[(&str, &str)] = &[
    ("bundl", "bundle"),
    ("prob", "probe"),
    ("dedup", "dedup"),
    ("judg", "judge"),
    ("verif", "verify"),
    ("synthes", "synthesis"),
];

/// Per-session state this spike tracks: just the `cwd` the client handed us
/// in `session/new`, keyed by the session id we minted for it.
type Sessions = Arc<Mutex<HashMap<SessionId, PathBuf>>>;

/// Entry point for `darkmux acp`. Builds its own tokio runtime and blocks on
/// the ACP stdio loop until the client (Zed) closes the connection — same
/// "sync `main`, async subsystem builds its own runtime" pattern
/// `darkmux serve` uses for its axum loop (`crates/darkmux-serve/src/
/// lib.rs::run`).
pub fn run() -> Result<i32> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime for `darkmux acp`")?;
    rt.block_on(serve())?;
    Ok(0)
}

async fn serve() -> Result<()> {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let next_session_ordinal = Arc::new(AtomicU64::new(1));

    let sessions_for_new = sessions.clone();
    let ordinal_for_new = next_session_ordinal.clone();
    let sessions_for_prompt = sessions.clone();

    Agent
        .builder()
        .on_receive_request(
            async move |initialize: InitializeRequest, responder, _cx| {
                // The operator specifically wants to know, empirically,
                // what protocol version Zed sends — log it unconditionally
                // to stderr (never stdout; see module docs).
                eprintln!(
                    "[darkmux-acp] initialize: client requested protocol version {}",
                    initialize.protocol_version
                );
                if let Some(info) = &initialize.client_info {
                    eprintln!(
                        "[darkmux-acp] initialize: client_info = {} {}",
                        info.name, info.version
                    );
                }
                responder.respond(
                    InitializeResponse::new(initialize.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, cx: ConnectionTo<Client>| {
                let ordinal = ordinal_for_new.fetch_add(1, Ordering::Relaxed);
                let session_id = SessionId::new(format!("darkmux-acp-{ordinal}"));
                sessions_for_new
                    .lock()
                    .expect("darkmux acp: sessions mutex poisoned")
                    .insert(session_id.clone(), request.cwd.clone());

                eprintln!(
                    "[darkmux-acp] session/new: {session_id} cwd={}",
                    request.cwd.display()
                );

                // Respond FIRST, then advertise commands — the wire order
                // matters. A `session/update` that reaches the client
                // before the `session/new` response names a session id the
                // client doesn't know yet; Zed drops exactly that update,
                // leaving its slash-command list empty ("Available commands
                // for darkmux: none", observed live). `Responder::respond`
                // enqueues synchronously, so calling it before
                // `send_notification` guarantees the response precedes the
                // update on the wire.
                responder.respond(NewSessionResponse::new(session_id.clone()))?;

                // (#1684) Registry-driven advertising — every mission
                // config in the merged registry (built-ins +
                // `~/.darkmux/mission-configs/`) that declares a `panel`
                // block, via `acp_panel::list_panel_commands` (the SAME
                // resolution `darkmux mission launch`/`mission status`
                // already use). This REPLACES the pre-#1684 hardcoded
                // single `/review` command — `review` is no longer special
                // here at all; it's advertised because the built-in
                // `review.json` now carries a `panel` block like any other
                // config would.
                let panel_commands = crate::acp_panel::list_panel_commands();
                eprintln!(
                    "[darkmux-acp] session/new: advertising {} panel command(s): {}",
                    panel_commands.len(),
                    panel_commands.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(", ")
                );
                let commands = AvailableCommandsUpdate::new(
                    panel_commands
                        .iter()
                        .map(|c| {
                            let cmd = AvailableCommand::new(c.id.clone(), c.description.clone());
                            match &c.hint {
                                Some(hint) => cmd.input(AvailableCommandInput::Unstructured(
                                    UnstructuredCommandInput::new(hint.clone()),
                                )),
                                None => cmd,
                            }
                        })
                        .collect::<Vec<_>>(),
                );
                cx.send_notification(SessionNotification::new(
                    session_id,
                    SessionUpdate::AvailableCommandsUpdate(commands),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, cx: ConnectionTo<Client>| {
                let session_id = request.session_id.clone();
                let text = extract_text(&request.prompt);

                // (#1684) Registry-driven command dispatch — replaces the
                // pre-#1684 hardcoded `is_review_command` string match.
                // `advertised` is recomputed HERE, per prompt, rather than
                // reused from `session/new` — the registry can change
                // between the two (an operator edits/adds a mission-config
                // file mid-session).
                let advertised = crate::acp_panel::list_panel_commands();
                let route = crate::acp_panel::parse_command(&text)
                    .and_then(|(cmd, args)| {
                        crate::acp_panel::route_command(&advertised, &cmd).map(|plan| (plan, args))
                    });

                let Some((plan, args)) = route else {
                    // Never hang, never bounce an error across the
                    // protocol boundary for an input we just don't support
                    // yet — reply plainly and end the turn. Lists the
                    // CURRENTLY advertised commands instead of hardcoding
                    // `/review`.
                    let _ = cx.send_notification(agent_chunk(
                        &session_id,
                        crate::acp_panel::not_a_command_message(&advertised),
                    ));
                    return responder.respond(PromptResponse::new(StopReason::EndTurn));
                };

                let cwd = sessions_for_prompt
                    .lock()
                    .expect("darkmux acp: sessions mutex poisoned")
                    .get(&session_id)
                    .cloned();

                let Some(cwd) = cwd else {
                    let _ = cx.send_notification(agent_chunk(
                        &session_id,
                        "darkmux acp: internal error — no working directory recorded for this \
                         session (was `session/new` skipped?). Start a new session and try \
                         again.",
                    ));
                    return responder.respond(PromptResponse::new(StopReason::EndTurn));
                };

                // Robustness rule (see the task brief): NOTHING from here
                // down may panic or propagate a hard error across the
                // protocol boundary. A crashed-looking agent in Zed has no
                // explanation; a chunk of error text does.
                let run_result = match plan {
                    // `review` keeps its EXISTING bespoke path, byte-for-byte
                    // unchanged (#1684 rule C) — only routed to differently.
                    // `config_id` (#1695 merge-gate MUST FIX) is the
                    // REGISTRY-RESOLVABLE id `route_command` decided on —
                    // never a hardcoded `"review"` — so a panel-advertised
                    // review VARIANT launches itself, not the built-in.
                    crate::acp_panel::RoutePlan::Review(config_id) => {
                        run_review(&session_id, &config_id, &args, &cwd, &cx).await
                    }
                    crate::acp_panel::RoutePlan::Ephemeral(config) => {
                        run_ephemeral_command(&session_id, *config, args, cwd.clone(), &cx).await
                    }
                    crate::acp_panel::RoutePlan::Launch(config_id) => {
                        run_launch_command(&session_id, &config_id, &args, &cwd, &cx).await
                    }
                };
                if let Err(err) = run_result {
                    eprintln!("[darkmux-acp] session/prompt: command failed: {err:#}");
                    let _ = cx.send_notification(agent_chunk(
                        &session_id,
                        format!("darkmux acp: command failed: {err:#}"),
                    ));
                }

                responder.respond(PromptResponse::new(StopReason::EndTurn))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(AcpStdio::new())
        .await?;

    Ok(())
}

/// Drives one review-family turn end to end: `git diff` → Plan update →
/// spawn the review subprocess → stream progress → final result chunk.
/// Returns `Err` only for genuinely internal failures (spawn failed, io
/// error, mutex poisoned upstream already handled) — the caller turns any
/// `Err` into a chunk instead of a protocol-level error.
///
/// `config_id` (#1695 merge-gate MUST FIX) is the REGISTRY-RESOLVABLE
/// mission-config id `acp_panel::route_command` decided this invocation
/// should launch — `RoutePlan::Review(String)`, never a hardcoded
/// `"review"` literal. Everything else here is byte-identical for the
/// plain `/review` case (where `config_id == "review"`); the only new
/// behavior is that a panel-advertised review VARIANT (an operator config
/// carrying `review.*` step kinds under a different id, e.g.
/// `review-lean`) spawns ITSELF instead of silently spawning the built-in
/// `review` config in its place.
///
/// `args` (#1695 merge-gate finding 4) is the raw text typed after the
/// command name. No review-family config consumes it today (the dedicated
/// review launcher, `mission_launch_review::launch`, declares no `args`
/// input), so trailing args are silently accepted by the ACP layer but
/// have no effect on the dispatched review — the final message says so
/// explicitly rather than leaving the operator to infer it from the
/// hint's "(no arguments)" text alone.
async fn run_review(session_id: &SessionId, config_id: &str, args: &str, cwd: &Path, cx: &ConnectionTo<Client>) -> Result<()> {
    let diff = git_diff(cwd).await?;
    if diff.trim().is_empty() {
        cx.send_notification(agent_chunk(
            session_id,
            "No uncommitted changes to review in this working tree.",
        ))?;
        return Ok(());
    }

    // (b) Plan update naming the review stages, sent up front so the
    // operator sees the graph immediately, before any model work starts.
    let mut plan_entries = initial_plan_entries();
    cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::Plan(Plan::new(plan_entries.clone())),
    ))?;

    let case_id = derive_case_id(cwd, &diff);
    let diff_path =
        std::env::temp_dir().join(format!("darkmux-acp-{}-{case_id}.diff", std::process::id()));
    tokio::fs::write(&diff_path, &diff)
        .await
        .with_context(|| format!("writing the diff to {}", diff_path.display()))?;

    let exe = std::env::current_exe().context("resolving darkmux's own executable path")?;
    let diff_file_arg = format!("diff_file={}", diff_path.display());
    let worktree_arg = format!("worktree={}", cwd.display());
    let case_id_arg = format!("case_id={case_id}");
    let bundler_param = choose_bundler(&diff);

    eprintln!(
        "[darkmux-acp] session/prompt: spawning `mission launch {config_id}` case={case_id} \
         diff_file={} bundler={}",
        diff_path.display(),
        bundler_param.as_deref().unwrap_or("(built-in)")
    );

    let mut cmd = Command::new(&exe);
    cmd.args([
        "mission",
        "launch",
        config_id,
        "--param",
        &diff_file_arg,
        "--param",
        &worktree_arg,
        "--param",
        &case_id_arg,
    ]);
    if let Some(bundler) = &bundler_param {
        cmd.args(["--param", bundler]);
    }
    let mut child = cmd
        .current_dir(cwd)
        .stdin(ProcStdio::null())
        .stdout(ProcStdio::piped())
        .stderr(ProcStdio::piped())
        .spawn()
        .with_context(|| format!("spawning `darkmux mission launch {config_id}` subprocess"))?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // Drain stdout in the background. THE CRITICAL CONSTRAINT: this is the
    // subprocess's stdout — never this process's own. Today the review
    // pipeline prints exactly one thing to stdout (the final rendered-
    // review JSON blob from `pr_review::emit_rendered`), so this task's job
    // is really just "buffer everything until the pipe closes, forwarding
    // anything that doesn't look like that JSON blob as a chunk too" (kept
    // uniform with the stderr handling below on the offhand chance that
    // changes). It deliberately does NOT touch stage-recognition state —
    // see the note on `current_stage` below for why that's safe today.
    let stdout_session_id = session_id.clone();
    let stdout_cx = cx.clone();
    let stdout_task: tokio::task::JoinHandle<String> = tokio::spawn(async move {
        let mut buf = String::new();
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if let Some(display) = forwardable_chunk_text(&line) {
                        let _ = stdout_cx.send_notification(agent_chunk(
                            &stdout_session_id,
                            display,
                        ));
                    }
                    buf.push_str(&line);
                    buf.push('\n');
                }
                Ok(None) => break,
                Err(err) => {
                    eprintln!("[darkmux-acp] reading subprocess stdout: {err}");
                    break;
                }
            }
        }
        buf
    });

    // Foreground: drain stderr line-by-line. Stage-transition tracking
    // (`current_stage`, the Plan entries, the per-stage ToolCall) lives
    // ONLY here, not in the stdout task above — the review pipeline's
    // progress markers (`[darkmux-liveness] ...`) only ever land on
    // stderr today, so there's no real concurrent-writer race to guard
    // against. If that ever changes, this state would need to move behind
    // a shared lock.
    let mut current_stage: Option<usize> = None;
    // Heartbeat rendering (operator finding, first live Zed run: the probe
    // stage runs for minutes and the stage card sat motionless — "looks
    // stuck. If I didn't have the ability to go hunting for evidence of
    // work on LM Studio I might be confused as a user"). The subprocess
    // already streams `[darkmux-liveness]` markers on stderr; render each
    // one into the active stage card's title so the card visibly ticks.
    let mut stage_marker_count: usize = 0;
    let mut last_liveness_title = String::new();
    let mut stderr_lines = BufReader::new(stderr).lines();
    loop {
        let line = match stderr_lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(err) => {
                eprintln!("[darkmux-acp] reading subprocess stderr: {err}");
                break;
            }
        };

        if let Some(stage_idx) = recognize_stage(&line) {
            if current_stage != Some(stage_idx) {
                advance_plan(&mut plan_entries, stage_idx);
                cx.send_notification(SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::Plan(Plan::new(plan_entries.clone())),
                ))?;

                let (_, label) = REVIEW_STAGES[stage_idx];
                cx.send_notification(SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::ToolCall(
                        ToolCall::new(stage_tool_call_id(label), format!("darkmux review — {label}"))
                            .kind(ToolKind::Execute)
                            .status(ToolCallStatus::InProgress),
                    ),
                ))?;
                current_stage = Some(stage_idx);
                stage_marker_count = 0;
            }
        }

        if let Some(marker) = liveness_marker(&line) {
            if let Some(stage_idx) = current_stage {
                stage_marker_count += 1;
                let (_, label) = REVIEW_STAGES[stage_idx];
                let title = format!("darkmux review — {label} · [{stage_marker_count}] {marker}");
                if title != last_liveness_title {
                    cx.send_notification(SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                            stage_tool_call_id(label),
                            ToolCallUpdateFields::new().title(title.clone()),
                        )),
                    ))?;
                    last_liveness_title = title;
                }
            }
        }

        if let Some(display) = forwardable_chunk_text(&line) {
            cx.send_notification(agent_chunk(session_id, display))?;
        }
    }

    let status = child
        .wait()
        .await
        .with_context(|| format!("waiting on the `{config_id}` subprocess"))?;
    let stdout_buf = stdout_task.await.unwrap_or_default();
    let _ = tokio::fs::remove_file(&diff_path).await;

    // (e) Close out the last stage's ToolCall + Plan, then the final
    // human-readable result.
    if let Some(stage_idx) = current_stage {
        let (_, label) = REVIEW_STAGES[stage_idx];
        let final_status = if status.success() {
            ToolCallStatus::Completed
        } else {
            ToolCallStatus::Failed
        };
        cx.send_notification(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                stage_tool_call_id(label),
                ToolCallUpdateFields::new().status(final_status),
            )),
        ))?;
    }
    if status.success() {
        for entry in &mut plan_entries {
            entry.status = PlanEntryStatus::Completed;
        }
        cx.send_notification(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::Plan(Plan::new(plan_entries)),
        ))?;
    }

    let mut final_text = render_final_message(&stdout_buf, status);
    // (#1695 merge-gate finding 4) No review-family config consumes `args`
    // today — say so explicitly rather than silently swallowing whatever
    // the operator typed after the command name.
    if !args.trim().is_empty() {
        final_text.push_str(&format!(
            "\n\n_(arguments `{}` were ignored — `{config_id}` takes none)_",
            args.trim()
        ));
    }
    cx.send_notification(agent_chunk(session_id, final_text))?;

    Ok(())
}

/// (#1684 rule D) Drive a procedural-only panel command's graph in-process
/// via `acp_panel::run_ephemeral` — no mission instance minted, no
/// lifecycle records. `run_ephemeral` is fully synchronous (it shells out
/// to `std::process::Command::output()` for `procedural.shell` steps), so
/// it runs on a `spawn_blocking` thread rather than the connection's own
/// async task — the same "never stall the ACP event loop" concern
/// `run_review`'s own module-doc note names for its (accepted, spike-grade)
/// blocking-in-place subprocess await.
///
/// `acp_panel::run_ephemeral` prints NOTHING to this process's own
/// stdout — the ACP wire — by construction (it never touches
/// `std::io::stdout`; `procedural.shell` captures its child's output via
/// `Command::output()`, and every flow record it emits rides
/// `crate::flow::record`, never a println). The rendered result reaches
/// Zed only via the `agent_chunk` notification below.
async fn run_ephemeral_command(
    session_id: &SessionId,
    config: crate::crew::mission_config::MissionConfig,
    args: String,
    cwd: PathBuf,
    cx: &ConnectionTo<Client>,
) -> Result<()> {
    let text = tokio::task::spawn_blocking(move || crate::acp_panel::run_ephemeral(&config, &args, &cwd))
        .await
        .context("joining the ephemeral panel-command task")??;
    cx.send_notification(agent_chunk(session_id, text))?;
    Ok(())
}

/// (#1684 rule D) Launch a panel command whose graph has at least one
/// model-dispatching step as a normal `darkmux mission launch <id>`
/// subprocess — a full instance, same pattern [`run_review`] uses for its
/// own subprocess (this process's own executable, re-invoked, cwd = the
/// session's cwd, stdout/stderr captured as pipes — never inherited, so
/// nothing but this file's own `agent_chunk` notifications reaches the ACP
/// wire). Unlike `run_review`, there is no bespoke stage/liveness parsing
/// here — that machinery is `review`'s own; a generic panel command
/// renders its subprocess's stdout (trimmed) as the final message on
/// success, or its stderr on failure. Sends an up-front "launched…" chunk
/// before awaiting the subprocess (#1684 QA finding — CONSIDER 12): unlike
/// `run_review`, which streams a `Plan` immediately, this route would
/// otherwise leave Zed showing nothing but a spinner for however long the
/// launched mission's own model dispatches take.
///
/// **`args` honesty note (#1684 QA finding — MUST-FIX 5).** The raw text
/// forwards as `--param args=<raw>` (omitted when empty) — the standard
/// `mission launch` CLI mechanism (`collect_inputs`). This is a
/// forward-compatible HOOK, not (yet) a wired delivery: today NO shipped
/// config declares `args` as a `MissionInput` or consumes it via
/// `task_overrides` (`build_launch_params` only produces overrides for the
/// coder-phase-shaped kinds), so a Launch-routed panel command that takes
/// arguments will see `mission launch` warn on stderr about an undeclared
/// input and the value will not reach any step's config. #1685's verb
/// configs are expected to declare `args` for real; wiring the actual
/// delivery mechanism (however #1685 chooses to shape it — task_overrides,
/// a generic step-config substitution, or something else) is that packet's
/// job, not this one's.
async fn run_launch_command(
    session_id: &SessionId,
    config_id: &str,
    args: &str,
    cwd: &Path,
    cx: &ConnectionTo<Client>,
) -> Result<()> {
    let exe = std::env::current_exe().context("resolving darkmux's own executable path")?;
    let mut cmd = Command::new(&exe);
    cmd.args(["mission", "launch", config_id]);
    if !args.trim().is_empty() {
        cmd.args(["--param", &format!("args={args}")]);
    }

    eprintln!(
        "[darkmux-acp] session/prompt: spawning `mission launch {config_id}` cwd={}",
        cwd.display()
    );
    let _ = cx.send_notification(agent_chunk(session_id, format!("darkmux: launching `{config_id}`…")));

    let output = cmd
        .current_dir(cwd)
        .stdin(ProcStdio::null())
        .stdout(ProcStdio::piped())
        .stderr(ProcStdio::piped())
        .output()
        .await
        .with_context(|| format!("spawning `darkmux mission launch {config_id}` subprocess"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let text = if output.status.success() {
        if stdout.is_empty() {
            format!("darkmux: `{config_id}` completed.")
        } else {
            stdout
        }
    } else {
        let detail = if stderr.is_empty() { &stdout } else { &stderr };
        format!("darkmux: `{config_id}` failed ({}).\n\n{detail}", output.status)
    };
    cx.send_notification(agent_chunk(session_id, text))?;
    Ok(())
}

fn initial_plan_entries() -> Vec<PlanEntry> {
    REVIEW_STAGES
        .iter()
        .map(|(_, label)| {
            PlanEntry::new(
                format!("darkmux review — {label}"),
                PlanEntryPriority::Medium,
                PlanEntryStatus::Pending,
            )
        })
        .collect()
}

/// Mark every entry before `stage_idx` Completed, `stage_idx` itself
/// InProgress, and leave later entries Pending. The protocol replaces the
/// whole plan on every update, so the full entry list is resent each call.
fn advance_plan(entries: &mut [PlanEntry], stage_idx: usize) {
    for (i, entry) in entries.iter_mut().enumerate() {
        entry.status = match i.cmp(&stage_idx) {
            std::cmp::Ordering::Less => PlanEntryStatus::Completed,
            std::cmp::Ordering::Equal => PlanEntryStatus::InProgress,
            std::cmp::Ordering::Greater => PlanEntryStatus::Pending,
        };
    }
}

fn stage_tool_call_id(stage_label: &str) -> ToolCallId {
    ToolCallId::new(format!("darkmux-review-{stage_label}"))
}

/// SPIKE-GRADE stage recognition (see module docs): lowercase-substring
/// match against [`REVIEW_STAGES`]. Returns the index of the first stage
/// whose stem appears in `line`.
fn recognize_stage(line: &str) -> Option<usize> {
    let lower = line.to_ascii_lowercase();
    REVIEW_STAGES.iter().position(|(stem, _)| lower.contains(stem))
}

/// What to forward to the ACP client as an `AgentMessageChunk`, if
/// anything: trims the line, skips blanks, and skips anything that looks
/// like a raw JSON payload (the one thing on stdout we specifically want
/// buffered-only, not dumped into the chat as a giant blob — see
/// `render_final_message`). Strips the `[darkmux-liveness] ` prefix when
/// present so the chat reads as prose instead of a log-file dump.
fn forwardable_chunk_text(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('{') {
        return None;
    }
    let display = trimmed.strip_prefix("[darkmux-liveness] ").unwrap_or(trimmed);
    Some(format!("`{display}`"))
}

/// Deterministic case id (no `Date`/random, per the task brief): the diff's
/// content hash plus the cwd's directory name, so repeated reviews of an
/// unchanged diff in the same workspace reuse the same case id while a
/// changed diff or a different workspace gets a new one.
fn derive_case_id(cwd: &Path, diff: &str) -> String {
    let hash = blake3::hash(diff.as_bytes());
    let short = &hash.to_hex().to_string()[..8];
    let base = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    format!("zed-{base}-{short}")
}

/// Compact human rendering of a `[darkmux-liveness]` stderr marker:
/// `<phase> · +<s>s` or `<phase> (<detail>) · +<s>s`; `None` for any other
/// line. Wire format per `darkmux_types::dispatch_liveness::emit`:
/// `[darkmux-liveness] <ts> +<ms>ms <phase> pid=<n> case=<id> | <detail>`
/// — the pid/case/timestamp are noise at this grain (the case is constant
/// for the whole run), but the `+<ms>ms` elapsed clock is exactly the
/// "is it moving" signal, so it renders as seconds.
fn liveness_marker(line: &str) -> Option<String> {
    let rest = line.strip_prefix("[darkmux-liveness] ")?;
    let mut tokens = rest.split_whitespace();
    let _ts = tokens.next()?;
    let elapsed_ms = tokens.next()?;
    let phase = tokens.next()?;
    let secs = elapsed_ms
        .strip_prefix('+')
        .and_then(|t| t.strip_suffix("ms"))
        .and_then(|t| t.parse::<u64>().ok())
        .map(|ms| ms / 1000);
    let detail = rest.split(" | ").nth(1).map(str::trim).filter(|d| !d.is_empty());
    let mut out = match detail {
        Some(d) => format!("{phase} ({d})"),
        None => phase.to_string(),
    };
    if let Some(s) = secs {
        out.push_str(&format!(" · +{s}s"));
    }
    Some(out)
}

/// SPIKE-GRADE bundler routing (see module docs): scan the diff's
/// `+++ b/` target paths — TypeScript anywhere wins the built-in bundler
/// (`None`); otherwise any `.edge` file routes to the operator's
/// `darkmux-bundler-edge` plugin. A mixed ts+edge diff takes the TS path
/// and the templates are skipped — good enough for a spike, wrong for
/// the real feature (bundler composition is a #1388 follow-up). Without
/// this routing, a template-only diff is a guaranteed degenerate close
/// ("3 skipped: non-code extension" — observed live on the first Zed
/// demo, 2026-08-07).
fn choose_bundler(diff: &str) -> Option<String> {
    let mut saw_edge = false;
    for line in diff.lines() {
        if let Some(target) = line.strip_prefix("+++ b/") {
            let t = target.trim();
            if t.ends_with(".ts") || t.ends_with(".tsx") {
                return None;
            }
            if t.ends_with(".edge") {
                saw_edge = true;
            }
        }
    }
    saw_edge.then(edge_bundler_param)
}

/// The `bundler=<cmd>` param value for the Edge plugin. Resolved to an
/// absolute path when the plugin exists at its conventional install
/// location (`~/.local/bin/darkmux-bundler-edge`) — the agent may be
/// running under Zed's GUI environment, whose PATH does not necessarily
/// include `~/.local/bin` — falling back to the bare PATH-resolved name.
fn edge_bundler_param() -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let p = Path::new(&home).join(".local/bin/darkmux-bundler-edge");
        if p.is_file() {
            return format!("bundler={}", p.display());
        }
    }
    "bundler=darkmux-bundler-edge".to_string()
}

async fn git_diff(cwd: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("diff")
        .arg("HEAD")
        .current_dir(cwd)
        .stdin(ProcStdio::null())
        .stdout(ProcStdio::piped())
        .stderr(ProcStdio::piped())
        .output()
        .await
        .with_context(|| format!("running `git diff HEAD` in {}", cwd.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "`git diff HEAD` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The review comment is rendered for its normal destination — a GitHub PR
/// comment, where embedded HTML renders. Zed's agent panel renders markdown
/// but NOT embedded HTML, so the one piece of HTML furniture the renderer
/// emits (the `<sub>…</sub>` small-print footer — `src/pr_review.rs`, its
/// only HTML) shows up in the panel as literal tags (operator, first live
/// run: "is that the github PR output leaking into the agent panel?" —
/// yes, verbatim). Translate exactly that furniture to markdown emphasis.
/// Deliberately a KNOWN-TAG translation, never a generic HTML stripper:
/// finding evidence legitimately quotes literal HTML (tonight's flagged
/// `<form …>` tag) and must pass through untouched.
fn panelize_comment(comment: &str) -> String {
    comment.replace("<sub>", "*").replace("</sub>", "*")
}

fn render_final_message(stdout_buf: &str, status: std::process::ExitStatus) -> String {
    let trimmed = stdout_buf.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(comment) = value.get("comment").and_then(|v| v.as_str()) {
            return panelize_comment(comment);
        }
        let mode = value.get("mode").and_then(|v| v.as_str()).unwrap_or("unknown");
        return format!(
            "darkmux review finished (mode: {mode}) but produced no renderable comment.\n\n\
             ```json\n{trimmed}\n```"
        );
    }
    if status.success() {
        format!(
            "darkmux review finished (exit 0) but its output didn't parse as the expected \
             JSON envelope. Raw stdout:\n\n```\n{trimmed}\n```"
        )
    } else {
        format!(
            "darkmux review failed ({status}). Raw stdout:\n\n```\n{trimmed}\n```"
        )
    }
}

fn extract_text(prompt: &[ContentBlock]) -> String {
    prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn agent_chunk(session_id: &SessionId, text: impl Into<String>) -> SessionNotification {
    SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(text.into()))),
    )
}
