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
//! - No cancellation support. `session/cancel` is unhandled — a real
//!   implementation would wire up `RequestCancellation` so the user's Zed
//!   "stop" button actually stops the subprocess. (#1684 Packet 2 — QA
//!   MUST-FIX: the `session/prompt` handler used to await the whole
//!   command in place, blocking the connection's dispatch loop for the
//!   duration; it now runs on a `cx.spawn`'d task instead, which this
//!   packet needed anyway so a gated command's `session/request_permission`
//!   round trip doesn't deadlock the very loop that would deliver its
//!   reply. The loop no longer blocks, but `session/cancel` still isn't
//!   wired to actually tear down the spawned task — that's the cancellation
//!   gap this bullet still names.)
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
    ContentChunk, InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
    PermissionOption, PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    PromptRequest, PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, SessionId,
    SessionNotification, SessionUpdate, StopReason, ToolCall, ToolCallContent, ToolCallId,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, UnstructuredCommandInput,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Stdio as AcpStdio};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
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

/// The no-slash channel's routing dispatch (#1698 Packet B), injectable so
/// `serve()` can be driven in a test over an in-process transport with a
/// CANNED router — never a live model — while `run()`'s production call
/// site wires the real `crate::radio::dispatch_router_call`. `Arc<dyn Fn>`
/// (not `radio::ModelCall`'s borrowed `FnMut`) because this needs to be
/// cloned into MULTIPLE `'static` `async move` closures registered on the
/// connection builder, one per `session/prompt` call, outliving `serve`'s
/// own stack frame.
type RouterCall = Arc<dyn Fn(&str) -> Result<String> + Send + Sync>;

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
    let router: RouterCall = Arc::new(crate::radio::dispatch_router_call);
    rt.block_on(serve(router, AcpStdio::new()))?;
    Ok(0)
}

/// `transport` is generic (#1698 Packet B test infrastructure) — production
/// (`run()` above) passes real `AcpStdio::new()`; pipe-level tests pass
/// `agent_client_protocol::ByteStreams::new(writer, reader)` over an
/// in-process `tokio::io::duplex`, so the SAME connection-handling code
/// this function builds runs in both, never a second test-only copy of the
/// handler chain.
async fn serve(
    router_call: RouterCall,
    transport: impl agent_client_protocol::ConnectTo<Agent> + 'static,
) -> Result<()> {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let next_session_ordinal = Arc::new(AtomicU64::new(1));

    let sessions_for_new = sessions.clone();
    let ordinal_for_new = next_session_ordinal.clone();
    let sessions_for_prompt = sessions.clone();
    let router_for_prompt = router_call.clone();

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
                let trimmed = text.trim();

                // (#1698 Packet B — "the slash becomes the mode bit")
                // Decided purely on the leading character, before any
                // catalog lookup or model call: empty/whitespace text is
                // UNCHANGED (never routes, never dispatches — the plain
                // "not a command" listing, same as pre-#1698); a leading
                // `/` is LAW — `acp_panel::parse_command`'s existing
                // deterministic match, unchanged from Packet 1 of #1684
                // except that bare-word matching is now retired (see that
                // function's own doc); anything else, non-empty and
                // slash-less, is the NEW no-slash interpreted channel
                // (`run_no_slash_route` below) — never a pattern match, a
                // small local routing seat's classification instead.
                if trimmed.is_empty() {
                    let advertised = crate::acp_panel::list_panel_commands();
                    let _ = cx.send_notification(agent_chunk(
                        &session_id,
                        crate::acp_panel::not_a_command_message(&advertised),
                    ));
                    return responder.respond(PromptResponse::new(StopReason::EndTurn));
                }

                if trimmed.starts_with('/') {
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

                    let Some(cwd) = session_cwd(&sessions_for_prompt, &session_id) else {
                        let _ = cx.send_notification(agent_chunk(&session_id, NO_CWD_MESSAGE));
                        return responder.respond(PromptResponse::new(StopReason::EndTurn));
                    };

                    // (#1684 Packet 2 — QA MUST-FIX) The actual command
                    // execution runs on a SEPARATELY SPAWNED task via
                    // `cx.spawn`, never `.await`ed inline in this closure's
                    // own future. Why this is load-bearing, not style: this
                    // closure's future is polled DIRECTLY inside the
                    // connection's incoming-message dispatch loop
                    // (`agent_client_protocol`'s `jsonrpc::incoming_actor`
                    // iterates incoming frames and does `dispatch_dispatch(...)
                    // .await?` — a plain inline await, not a spawn — for every
                    // Request entry; that SAME loop is also the only place an
                    // incoming Response entry gets routed to a pending
                    // `SentRequest`). A gated command's `acp_gate_handler`
                    // blocks (via `spawn_blocking` + a channel recv) waiting
                    // for the client's `session/request_permission` REPLY —
                    // and that reply can only ever be delivered by THIS loop.
                    // Awaiting the command inline here would therefore
                    // deadlock: the loop can't process the incoming reply that
                    // would unblock the very future it's still awaiting (the
                    // crate's own `SentRequest::block_task` docs name exactly
                    // this failure mode as "Unsafe Usage (in handlers — will
                    // deadlock!)", and its "Safe Usage" shape is precisely
                    // "spawn a task, respond independently" — moving the WHOLE
                    // command, `responder` included, into `cx.spawn` is that
                    // shape applied to a response that itself depends on the
                    // round trip's outcome, not just the round trip alone).
                    // `cx.spawn` returns as soon as the task is REGISTERED
                    // (not once it finishes), so this closure's own future
                    // resolves immediately either way — the dispatch loop is
                    // free again right away, gated or not.
                    //
                    // Side effect (intentional, not just tolerated): this also
                    // retires the pre-#1684 spike limitation this module's own
                    // doc named ("the `session/prompt` handler awaits the
                    // whole review subprocess in place, blocking the
                    // connection's event loop for the duration") — `review`'s
                    // own multi-minute subprocess run no longer blocks the
                    // loop from handling other sessions/notifications either.
                    let cx_task = cx.clone();
                    return cx.spawn(async move {
                        // Robustness rule (see the task brief): NOTHING from
                        // here down may panic or propagate a hard error across
                        // the protocol boundary. A crashed-looking agent in
                        // Zed has no explanation; a chunk of error text does.
                        let run_result = execute_route_plan(&session_id, plan, &args, &cwd, &cx_task).await;
                        if let Err(err) = run_result {
                            eprintln!("[darkmux-acp] session/prompt: command failed: {err:#}");
                            let _ = cx_task.send_notification(agent_chunk(
                                &session_id,
                                format!("darkmux acp: command failed: {err:#}"),
                            ));
                        }

                        responder.respond(PromptResponse::new(StopReason::EndTurn))
                    });
                }

                // (#1698 Packet B) The no-slash interpreted channel. Same
                // "spawn the whole thing, never await inline" shape as the
                // slash path above and for the SAME reason (the routing
                // dispatch itself is a synchronous, potentially slow call —
                // see `run_no_slash_route`'s own doc on why it must not
                // block other sessions).
                let Some(cwd) = session_cwd(&sessions_for_prompt, &session_id) else {
                    let _ = cx.send_notification(agent_chunk(&session_id, NO_CWD_MESSAGE));
                    return responder.respond(PromptResponse::new(StopReason::EndTurn));
                };
                let cx_task = cx.clone();
                let router_for_task = router_for_prompt.clone();
                cx.spawn(async move {
                    let run_result =
                        run_no_slash_route(&session_id, &text, &cwd, &cx_task, router_for_task).await;
                    if let Err(err) = run_result {
                        eprintln!("[darkmux-acp] session/prompt: no-slash route failed: {err:#}");
                        let _ = cx_task.send_notification(agent_chunk(
                            &session_id,
                            format!("darkmux acp: command failed: {err:#}"),
                        ));
                    }
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(transport)
        .await?;

    Ok(())
}

/// The one message both cwd-lookup guards in `serve()`'s `PromptRequest`
/// handler send — factored so the slash and no-slash branches (#1698
/// Packet B) don't carry two copies of the same literal.
const NO_CWD_MESSAGE: &str = "darkmux acp: internal error — no working directory recorded for this \
     session (was `session/new` skipped?). Start a new session and try again.";

/// Look up a session's recorded `cwd` — the same `sessions.lock()...get(...)`
/// pattern both `PromptRequest` branches (#1698 Packet B: the slash path
/// and the no-slash path) need before they can execute anything.
fn session_cwd(sessions: &Sessions, session_id: &SessionId) -> Option<PathBuf> {
    sessions
        .lock()
        .expect("darkmux acp: sessions mutex poisoned")
        .get(session_id)
        .cloned()
}

/// Turn a resolved [`crate::acp_panel::RoutePlan`] into an actual execution
/// — the SAME three-way match `serve()`'s `PromptRequest` handler ran
/// inline before #1698 Packet B, extracted so BOTH the slash-command path
/// AND the new no-slash channel (`run_no_slash_route` below) drive
/// identical behavior once a plan is resolved: same `run_review`/
/// `run_ephemeral_command`/`run_launch_command` execution primitives, same
/// gates, no divergence between "the operator typed `/review`" and "the
/// operator typed `review this` and the router picked `/review`".
async fn execute_route_plan(
    session_id: &SessionId,
    plan: crate::acp_panel::RoutePlan,
    args: &str,
    cwd: &Path,
    cx: &ConnectionTo<Client>,
) -> Result<()> {
    match plan {
        // `review` keeps its EXISTING bespoke path, byte-for-byte
        // unchanged (#1684 rule C) — only routed to differently.
        // `config_id` (#1695 merge-gate MUST FIX) is the
        // REGISTRY-RESOLVABLE id `route_command` decided on —
        // never a hardcoded `"review"` — so a panel-advertised
        // review VARIANT launches itself, not the built-in.
        crate::acp_panel::RoutePlan::Review(config_id) => {
            run_review(session_id, &config_id, args, cwd, cx).await
        }
        crate::acp_panel::RoutePlan::Ephemeral(config) => {
            run_ephemeral_command(session_id, *config, args.to_string(), cwd.to_path_buf(), cx).await
        }
        crate::acp_panel::RoutePlan::Launch(config_id) => {
            run_launch_command(session_id, &config_id, args, cwd, cx).await
        }
    }
}

/// **The no-slash interpreted channel (#1698 Packet B).** Free text — no
/// leading slash, non-empty (both already decided by `serve()`'s caller
/// before this is reached) — routes through `crate::radio::
/// route_and_record`, the SAME closed-set router `darkmux radio` (the CLI
/// verb, #1698 Packet A) uses. Reused, never forked: this function calls
/// INTO `radio.rs`'s core exactly like `radio_cli.rs` does, so a
/// description-quality fix or a catalog change benefits both surfaces from
/// one edit.
///
/// **Ordering (issue #1698: "a successful route sends the provenance chunk
/// FIRST"):** on [`crate::radio::RouteDecision::Route`], the "routed to
/// /x — from your text" chunk is sent BEFORE [`execute_route_plan`] runs —
/// the operator sees WHERE their sentence went before seeing what it did,
/// same provenance contract the CLI's own `radio: routing to /x — from
/// your text` line gives (wall 4: "provenance boxes invisibility").
/// Execution then runs through the EXACT SAME `RoutePlan` machinery a
/// slash invocation uses — identical behavior, identical gates; a routed
/// `/pr-merge` still hits the native sign-off dialog.
///
/// **On [`crate::radio::RouteDecision::Refuse`]:** the refusal reason is
/// rendered VERBATIM (persona-bearing operator content — the operator's
/// own TARS-persona role override, when one is installed, speaks here
/// exactly as it does on the CLI; see `radio-router.md`'s own doc on "the
/// voice may only live where prose already lives") followed by the live
/// command listing — same TWO-PART SHAPE `radio_cli.rs::run`'s own refusal
/// rendering uses (reason, then listing), though the listing's own wording
/// differs: this panel path reuses `acp_panel::not_a_command_message`
/// ("darkmux acp doesn't recognize that as a command. Available commands:
/// …"), while the CLI's `advertised_list_message` is plainer ("Available
/// commands: …") — each surface's EXISTING listing helper, not a new
/// third rendering invented for this channel.
///
/// **Never blocks other sessions:** the routing dispatch itself
/// (`router_call` — `crate::radio::dispatch_router_call` in production,
/// ultimately a blocking HTTP call; see `dispatch_local_single_shot`'s own
/// doc for why it's container-free but still synchronous) runs on
/// `tokio::task::spawn_blocking`, never inline on this async task — the
/// SAME "never stall the ACP event loop" shape `run_ephemeral_command`'s
/// own doc names for the ephemeral graph runner. Wall 4's flow record
/// (source text + chosen route/refusal + surface) is emitted from
/// `radio::route_and_record` itself — the shared core both this function
/// and the CLI verb call into — never duplicated here.
///
/// `router_call` is injected (`RouterCall`, not the hardcoded
/// `radio::dispatch_router_call`) so pipe-level ACP tests can drive this
/// whole channel with a CANNED router — no live model dispatch ever runs
/// under test (see `serve`'s own doc + the `tests` module below).
async fn run_no_slash_route(
    session_id: &SessionId,
    text: &str,
    cwd: &Path,
    cx: &ConnectionTo<Client>,
    router_call: RouterCall,
) -> Result<()> {
    let text_owned = text.to_string();
    let decision = tokio::task::spawn_blocking(move || {
        let catalog = crate::radio::compile_catalog();
        crate::radio::route_and_record(&text_owned, &catalog, crate::radio::RadioSurface::Panel, &mut |message: &str| {
            (router_call)(message)
        })
    })
    .await
    .context("joining the radio routing task")?;

    match decision {
        crate::radio::RouteDecision::Refuse { reason } => {
            let advertised = crate::acp_panel::list_panel_commands();
            cx.send_notification(agent_chunk(
                session_id,
                format!("{reason}\n\n{}", crate::acp_panel::not_a_command_message(&advertised)),
            ))?;
            Ok(())
        }
        crate::radio::RouteDecision::Route { command, args } => {
            cx.send_notification(agent_chunk(
                session_id,
                format!("darkmux: routing to /{command} — from your text"),
            ))?;
            let advertised = crate::acp_panel::list_panel_commands();
            let plan = crate::acp_panel::route_command(&advertised, &command).ok_or_else(|| {
                anyhow::anyhow!(
                    "darkmux acp: routed command `{command}` is no longer advertised (the \
                     registry changed between routing and execution)"
                )
            })?;
            execute_route_plan(session_id, plan, &args, cwd, cx).await
        }
    }
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
    // (#1684 Packet 2) The ACP surface's operator sign-off gate handler —
    // see `acp_gate_handler`'s own doc. Built here (on the connection's
    // async task, which owns `cx`/`session_id`) and moved into the
    // `spawn_blocking` closure below; the ephemeral runner calls it
    // synchronously from the blocking thread for any step in `config`'s
    // graph that declares `"gate": "operator"`.
    let mut gate = acp_gate_handler(cx.clone(), session_id.clone());
    let outcome = tokio::task::spawn_blocking(move || {
        crate::acp_panel::run_ephemeral(&config, &args, &cwd, Some(&mut gate))
    })
    .await
    .context("joining the ephemeral panel-command task")??;
    // The ACP panel surface has no exit-code concept — it just displays
    // whichever text comes back, byte-identical to before `run_ephemeral`
    // gained a typed `success` field (#1698 Packet B carry-list item 5).
    cx.send_notification(agent_chunk(session_id, outcome.text))?;
    Ok(())
}

/// (#1684 Packet 2) Build the ACP surface's operator sign-off gate handler
/// — the `darkmux_crew::gate::GateHandler` the ephemeral runner invokes,
/// via `darkmux_crew::scheduler::run_step_graph`, for any step whose
/// `gate` field is `"operator"`.
///
/// The returned closure is `FnMut` but runs SYNCHRONOUSLY on a
/// `spawn_blocking` thread (see `run_ephemeral`'s own doc on why the
/// ephemeral runner is blocking, and `run_ephemeral_command` above for
/// where this closure gets handed in) — it cannot itself `.await` the ACP
/// round-trip. Per call it: (1) builds a `session/request_permission`
/// request naming the step + rendering its composed input facts as the
/// dialog body, with two options (allow/reject) — the #1685 spec's "ACP →
/// native session/request_permission dialog"; (2) spawns a NEW async task
/// via `cx.spawn` that awaits the response and forwards the decision back
/// over a one-shot `std::sync::mpsc` channel; (3) blocks (a plain
/// synchronous `Receiver::recv`, fine here — we are ALREADY on a
/// `spawn_blocking` thread, never the connection's own dispatch-loop task)
/// until that decision arrives.
///
/// **Why the `cx.spawn` in step (2) is actually safe here (read before
/// touching this).** `SentRequest::block_task`'s own doc calls calling it
/// directly inside a request handler an "Unsafe Usage… will deadlock" —
/// the deadlock is real, and the crate's own `incoming_actor` loop is why:
/// it `.await`s every `on_receive_request` handler INLINE (one at a time),
/// and that SAME loop is the only place an incoming response gets routed
/// back to a pending `block_task().await`. Spawning `block_task` alone,
/// while the caller of THIS handler is still `.await`ing this whole
/// closure inline in that loop, would NOT escape the deadlock — the
/// spawned task's reply still can't be delivered until the loop is free,
/// and the loop isn't free until the caller's await resolves. What
/// actually makes this safe is the CALLER: `src/acp.rs`'s `PromptRequest`
/// handler moves the ENTIRE command (this handler's caller chain included)
/// into ITS OWN `cx.spawn`, with `responder` carried into that spawned
/// task rather than the handler's own inline future — see that call
/// site's doc for the full reasoning. That is what frees the dispatch
/// loop early, which is what lets the loop actually deliver the
/// `session/request_permission` response this function's own `cx.spawn`
/// is waiting on. This function's `cx.spawn` alone, without that caller-
/// side restructure, would still deadlock.
///
/// Fails closed (Declined) if `cx.spawn` itself errors (the connection is
/// closing) or the response channel is dropped before a decision arrives
/// (the spawned task panicked, or `session/cancel` tore down the turn) —
/// never silently approves on any of those paths. No timeout is enforced
/// on the wait itself (#1684 QA CONSIDER — a stalled/never-responding
/// client hangs this gate indefinitely rather than failing closed on a
/// deadline; acceptable for a v1 given the dispatch loop itself is no
/// longer at risk, worth revisiting once a real timeout mechanism exists
/// elsewhere in this file to mirror).
fn acp_gate_handler(
    cx: ConnectionTo<Client>,
    session_id: SessionId,
) -> impl FnMut(&crate::crew::types::Step, &BTreeMap<String, String>) -> crate::crew::gate::GateDecision {
    move |step, facts| {
        let step_id = step.id.clone();
        let facts_text = render_gate_facts(facts);
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<crate::crew::gate::GateDecision>();
        let cx2 = cx.clone();
        let session_id2 = session_id.clone();
        let step_id2 = step_id.clone();
        let spawn_result = cx.spawn(async move {
            let decision = request_operator_sign_off(&cx2, &session_id2, &step_id2, &facts_text).await;
            // The blocking side may have given up waiting (channel dropped)
            // if this task somehow outlives it — a dropped-receiver send
            // error is not this task's problem to report.
            let _ = resp_tx.send(decision);
            Ok(())
        });
        if let Err(e) = spawn_result {
            return crate::crew::gate::GateDecision::Declined {
                reason: format!(
                    "step `{step_id}` — could not schedule the operator sign-off request on the \
                     ACP connection: {e}"
                ),
            };
        }
        match resp_rx.recv() {
            Ok(decision) => decision,
            Err(_) => crate::crew::gate::GateDecision::Declined {
                reason: format!(
                    "step `{step_id}` — the sign-off response channel closed before Zed replied \
                     (the connection may have closed, or the turn was cancelled)"
                ),
            },
        }
    }
}

/// Render a step's composed upstream input facts as the `session/
/// request_permission` dialog body — one `key: value` line per fact,
/// sorted (the map is already a `BTreeMap`, so iteration order IS sort
/// order — no separate sort needed). A step with no upstream facts (e.g.
/// the FIRST step in a panel command's graph) still gets a dialog, just
/// with an explicit "no upstream facts" line rather than a blank body.
fn render_gate_facts(facts: &BTreeMap<String, String>) -> String {
    if facts.is_empty() {
        return "(no upstream facts)".to_string();
    }
    facts.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n")
}

/// Raise `session/request_permission` to the connected client and await its
/// decision — the async half of [`acp_gate_handler`]. Runs on a freshly
/// spawned, concurrent task (never inline inside a request handler — see
/// `acp_gate_handler`'s own doc on the deadlock this avoids).
async fn request_operator_sign_off(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    step_id: &str,
    facts_text: &str,
) -> crate::crew::gate::GateDecision {
    const ALLOW: &str = "allow";
    const REJECT: &str = "reject";

    // (#1684 QA CONSIDER) This `ToolCallId` is never announced via a prior
    // `SessionUpdate::ToolCall` before this request — unlike `run_review`'s
    // stage tool calls (`stage_tool_call_id`), which always send a
    // `ToolCall` notification before referencing that id again. Per the
    // schema, `RequestPermissionRequest.tool_call` is a `ToolCallUpdate`
    // (an upsert, not an update-only reference), so this SHOULD be fine on
    // a spec-compliant client — but this is the same class of "Zed drops
    // a message naming something it doesn't know about yet" surprise the
    // Packet-1 wire-ordering finding hit for `AvailableCommandsUpdate` (see
    // `session/new`'s handler comment above). Live dogfood verified the
    // dialog renders — for the FIRST gated invocation only, which is why
    // the id below carries a nonce:
    //
    // (#1684, confirmed live) A DETERMINISTIC id here collides on the
    // second gated invocation in one session: the first dialog's tool call
    // is already terminal under the same id, Zed renders no new dialog,
    // and the agent blocks forever on a reply that cannot come (the
    // operator's canonical approve-then-merge pair hit this on its first
    // real use). A process-global counter makes every request's id unique.
    static GATE_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = GATE_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tool_call = ToolCallUpdate::new(
        ToolCallId::new(format!("darkmux-gate-{step_id}-{nonce}")),
        ToolCallUpdateFields::new()
            .title(format!("darkmux — operator sign-off required: `{step_id}`"))
            .kind(ToolKind::Execute)
            .status(ToolCallStatus::Pending)
            .content(vec![ToolCallContent::from(facts_text.to_string())]),
    );
    let options = vec![
        PermissionOption::new(ALLOW, "Allow", PermissionOptionKind::AllowOnce),
        PermissionOption::new(REJECT, "Reject", PermissionOptionKind::RejectOnce),
    ];
    let request = RequestPermissionRequest::new(session_id.clone(), tool_call, options);

    match cx.send_request(request).block_task().await {
        Ok(response) => match response.outcome {
            RequestPermissionOutcome::Selected(sel) if &*sel.option_id.0 == ALLOW => {
                crate::crew::gate::GateDecision::Approved
            }
            RequestPermissionOutcome::Selected(sel) => crate::crew::gate::GateDecision::Declined {
                reason: format!(
                    "step `{step_id}` — operator selected `{}` at the sign-off dialog",
                    sel.option_id
                ),
            },
            RequestPermissionOutcome::Cancelled => crate::crew::gate::GateDecision::Declined {
                reason: format!("step `{step_id}` — the sign-off request was cancelled"),
            },
            // `RequestPermissionOutcome` is `#[non_exhaustive]` (the schema
            // crate may add a variant in a future minor release this
            // binary's pinned version predates) — an outcome this match
            // doesn't recognize is exactly a "no sign-off received" case,
            // so it fails closed like `Cancelled` rather than panicking on
            // an unmatched arm.
            _ => crate::crew::gate::GateDecision::Declined {
                reason: format!(
                    "step `{step_id}` — the client returned an unrecognized sign-off outcome"
                ),
            },
        },
        Err(e) => crate::crew::gate::GateDecision::Declined {
            reason: format!("step `{step_id}` — the sign-off request to the client failed: {e}"),
        },
    }
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
///
/// **A gated step in this route can never be approved (#1684 QA
/// CONSIDER, a deliberate boundary, not a bug).** The subprocess below
/// spawns with `stdin(ProcStdio::null())` — headless by construction — so
/// `mission_launch::cli_gate_handler` always resolves to the
/// non-interactive `refusal_handler`, and any `"gate": "operator"` step
/// in this config's graph refuses itself immediately. This is CORRECT
/// fail-closed behavior (never silently ungated), but it is also an
/// invisible capability boundary worth naming explicitly: a panel command
/// whose graph mixes a model-dispatching step (which routes it here, past
/// `is_procedural_only`) WITH a gated step is structurally unapprovable
/// from the panel today. Every gated example verb #1685 documents
/// (`pr-merge`, `pr-approve`) is procedural-only by design, so it takes
/// the ACP ephemeral route (`run_ephemeral_command`, with the real
/// `session/request_permission` handler) instead — this boundary is not
/// expected to bite in practice, but a future config author combining the
/// two would silently lose the ability to approve, so it's named here for
/// when that's revisited.
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
///
/// `pub(crate)` (#1698 Packet B carry-list item 1) — reused by
/// `synthesize_review_launch_params` below, which `src/radio_cli.rs`'s CLI
/// review route calls into rather than re-deriving the same hash.
pub(crate) fn derive_case_id(cwd: &Path, diff: &str) -> String {
    let hash = blake3::hash(diff.as_bytes());
    let short = &hash.to_hex().to_string()[..8];
    let base = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    format!("zed-{base}-{short}")
}

/// The `--param` strings + written diff tempfile a review-route launch
/// needs — `run_review`'s own required inputs (`diff_file`, `worktree`,
/// `case_id`, optional `bundler`), extracted into a pure(-ish; one tempfile
/// write) synchronous helper so `src/radio_cli.rs`'s CLI review route can
/// synthesize the SAME inputs the panel's `run_review` builds inline below
/// (#1698 Packet B carry-list item 1, the #1701 merge-gate headline
/// finding: `radio "review this"` used to route correctly then die at the
/// launcher's missing-inputs error because the CLI path never built these
/// params at all). `run_review` itself is left untouched — its own inline
/// construction still uses `tokio::fs::write` (async-friendly inside its
/// already-async function); this helper exists for a caller that has no
/// tokio runtime to run inside, and reuses [`derive_case_id`] /
/// [`choose_bundler`] rather than re-deriving either.
pub(crate) struct ReviewLaunchParams {
    /// The tempfile path the diff was written to — the caller must clean
    /// this up after the launched subprocess exits (`run_review`'s own
    /// `tokio::fs::remove_file` is the async-context precedent).
    pub diff_path: PathBuf,
    pub diff_file_arg: String,
    pub worktree_arg: String,
    pub case_id_arg: String,
    pub bundler_param: Option<String>,
}

/// Build a [`ReviewLaunchParams`] from an already-fetched `diff` string and
/// the session's `cwd` — writes the diff to a tempfile (the one I/O side
/// effect) and derives the case id + bundler choice from its content.
/// `diff` empty/whitespace-only is the caller's OWN "nothing to review"
/// check (mirroring `run_review`'s own early return) — this function does
/// not special-case it, since a synthesized `case_id`/`diff_path` for an
/// empty diff is harmless, just pointless; the caller decides whether to
/// call this at all.
pub(crate) fn synthesize_review_launch_params(cwd: &Path, diff: &str) -> Result<ReviewLaunchParams> {
    let case_id = derive_case_id(cwd, diff);
    let diff_path =
        std::env::temp_dir().join(format!("darkmux-acp-{}-{case_id}.diff", std::process::id()));
    std::fs::write(&diff_path, diff)
        .with_context(|| format!("writing the diff to {}", diff_path.display()))?;
    Ok(ReviewLaunchParams {
        diff_file_arg: format!("diff_file={}", diff_path.display()),
        worktree_arg: format!("worktree={}", cwd.display()),
        case_id_arg: format!("case_id={case_id}"),
        bundler_param: choose_bundler(diff),
        diff_path,
    })
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
///
/// `pub(crate)` (#1698 Packet B carry-list item 1) — reused by
/// `synthesize_review_launch_params` below, same reason as
/// [`derive_case_id`].
pub(crate) fn choose_bundler(diff: &str) -> Option<String> {
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

/// Pipe-level ACP tests (#1698 Packet B). `serve()` runs over an in-process
/// `agent_client_protocol::ByteStreams` transport (`tokio::io::duplex` —
/// the SAME "two in-memory duplex pairs" pattern the `agent-client-protocol`
/// crate's own integration tests use, e.g. `tests/jsonrpc_hello.rs`'s
/// `setup_test_streams`), never a real subprocess: the test drives the
/// OTHER end with raw newline-delimited JSON-RPC — exactly the wire shape
/// `AcpStdio` speaks over real stdio, empirically confirmed by this
/// packet's own investigation piping `darkmux acp` directly (see `serve`'s
/// module-level doc and `run_no_slash_route`'s doc for the design this
/// verifies). **No live model dispatch ever runs**: every test injects a
/// CANNED `router` closure via `spawn_test_agent` — the SAME `RouterCall`
/// seam `serve()`/`run_no_slash_route` take in production, wired here to a
/// closure instead of `crate::radio::dispatch_router_call`.
#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::ByteStreams;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    /// RAII env-var guard — isolates `DARKMUX_CREW_DIR`/`DARKMUX_FLOWS_DIR`
    /// to a fresh tempdir for one test, restoring the prior value on
    /// `Drop` (including on panic/early-return, unlike the manual
    /// save-then-restore-at-the-bottom pattern used elsewhere in this
    /// codebase). Every test in this module is `#[serial_test::serial]` —
    /// the SAME global lock every other env-mutating test in this binary
    /// already uses, so these never race a sibling test's own override.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: caller's test is #[serial_test::serial].
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: caller's test is #[serial_test::serial].
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// Write one panel-advertised, procedural-only fixture command —
    /// `id`'s `panel` block advertises it; its single `procedural.noop`
    /// step's `output` is the fixed string every scenario asserts on, so a
    /// test can distinguish "the command actually ran" from "something
    /// else happened" without any real dispatch.
    fn write_echo_fixture(crew_dir: &Path, id: &str, output: &str) {
        let dir = crew_dir.join("mission-configs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_string(&serde_json::json!({
                "id": id,
                "name": id,
                "panel": {"description": "Pipe-level test fixture — echoes a fixed string."},
                "phases": [{
                    "id": "p1",
                    "tasks": [{"id": "t1", "steps": [{"id": "s1", "kind": "procedural.noop", "config": {"output": output}}]}]
                }]
            }))
            .unwrap(),
        )
        .unwrap();
    }

    /// Spawn `serve()` over an in-process duplex pipe with the given
    /// CANNED `router` — the entire no-slash channel's model-facing
    /// surface under test, `Arc`-wrapped into the SAME `RouterCall` seam
    /// production wires to `crate::radio::dispatch_router_call`. Returns
    /// the test's own end of the pipe: a raw writer + a buffered reader,
    /// driven with plain newline-delimited JSON exactly like a real ACP
    /// client would over stdio.
    fn spawn_test_agent(
        router: impl Fn(&str) -> Result<String> + Send + Sync + 'static,
    ) -> (DuplexStream, BufReader<DuplexStream>) {
        let (test_writer, agent_reader) = tokio::io::duplex(64 * 1024);
        let (agent_writer, test_reader) = tokio::io::duplex(64 * 1024);
        let router_call: RouterCall = Arc::new(router);
        let transport = ByteStreams::new(agent_writer.compat_write(), agent_reader.compat());
        tokio::spawn(async move {
            let _ = serve(router_call, transport).await;
        });
        (test_writer, BufReader::new(test_reader))
    }

    async fn send_json(writer: &mut DuplexStream, value: serde_json::Value) {
        let mut bytes = serde_json::to_vec(&value).expect("test value serializes");
        bytes.push(b'\n');
        writer.write_all(&bytes).await.expect("writing to the test duplex");
        writer.flush().await.expect("flushing the test duplex");
    }

    async fn recv_json(reader: &mut BufReader<DuplexStream>) -> serde_json::Value {
        let mut line = String::new();
        let n = tokio::time::timeout(std::time::Duration::from_secs(10), reader.read_line(&mut line))
            .await
            .expect("timed out waiting for a line from the agent")
            .expect("reading a line from the test duplex");
        assert!(n > 0, "the agent side closed the connection before sending the expected line");
        serde_json::from_str(line.trim()).expect("agent emitted a non-JSON line")
    }

    /// `initialize` + `session/new` — every scenario's shared prelude.
    /// Returns the minted `sessionId`, after draining the
    /// `AvailableCommandsUpdate` notification `session/new` always sends.
    async fn handshake(writer: &mut DuplexStream, reader: &mut BufReader<DuplexStream>, cwd: &Path) -> String {
        send_json(
            writer,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": 1, "clientCapabilities": {}}
            }),
        )
        .await;
        let _init_response = recv_json(reader).await;

        send_json(
            writer,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/new",
                "params": {"cwd": cwd.to_string_lossy(), "mcpServers": []}
            }),
        )
        .await;
        let new_response = recv_json(reader).await;
        let session_id = new_response["result"]["sessionId"]
            .as_str()
            .expect("session/new must return a sessionId")
            .to_string();
        let _available_commands_update = recv_json(reader).await;
        session_id
    }

    async fn send_prompt(writer: &mut DuplexStream, session_id: &str, text: &str) {
        send_json(
            writer,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": text}]}
            }),
        )
        .await;
    }

    fn chunk_text(notification: &serde_json::Value) -> &str {
        notification["params"]["update"]["content"]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("expected an agent_message_chunk notification, got: {notification}"))
    }

    fn assert_end_turn(response: &serde_json::Value) {
        assert_eq!(
            response["result"]["stopReason"], "end_turn",
            "expected the session/prompt response to end the turn: {response}"
        );
    }

    /// (#1698 Packet B) The no-slash channel's core contract: a successful
    /// route sends the PROVENANCE chunk FIRST, then the executed command's
    /// OUTPUT — never the reverse, and never interleaved. Also asserts
    /// wall 4's flow record landed (source text + chosen command +
    /// surface=panel).
    #[tokio::test]
    #[serial_test::serial]
    async fn no_slash_route_sends_provenance_before_output_and_records_wall_4() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        let flows_tmp = tempfile::TempDir::new().unwrap();
        let _flows_guard = EnvGuard::set("DARKMUX_FLOWS_DIR", flows_tmp.path());
        write_echo_fixture(crew_tmp.path(), "echo-fixture", "fixture output");

        let router = |_msg: &str| -> Result<String> {
            Ok("```json\n{\"command\": \"echo-fixture\", \"args\": \"\"}\n```".to_string())
        };
        let (mut writer, mut reader) = spawn_test_agent(router);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        send_prompt(&mut writer, &session_id, "please give me the fixture").await;

        let provenance = recv_json(&mut reader).await;
        assert!(
            chunk_text(&provenance).contains("routing to /echo-fixture"),
            "provenance chunk must arrive FIRST, naming the routed command: {}",
            chunk_text(&provenance)
        );

        let output = recv_json(&mut reader).await;
        assert_eq!(chunk_text(&output), "fixture output", "the SECOND chunk is the executed command's own output");

        let final_response = recv_json(&mut reader).await;
        assert_end_turn(&final_response);

        let day = darkmux_flow::day_utc_now();
        let flow_path = flows_tmp.path().join(format!("{day}.jsonl"));
        let flow_contents = std::fs::read_to_string(&flow_path).expect("wall 4's flow record file must exist");
        assert!(flow_contents.contains("\"action\":\"radio.route\""), "{flow_contents}");
        assert!(flow_contents.contains("\"surface\":\"panel\""), "{flow_contents}");
        assert!(flow_contents.contains("\"command\":\"echo-fixture\""), "{flow_contents}");
        assert!(
            flow_contents.contains("please give me the fixture"),
            "the flow record must carry the source text: {flow_contents}"
        );
    }

    /// (#1698 Packet B) A refusal renders the (persona-bearing) reason
    /// verbatim plus the live command listing — and still records wall 4's
    /// flow record (as a refusal, not a route).
    #[tokio::test]
    #[serial_test::serial]
    async fn no_slash_refusal_renders_reason_and_listing_and_records_wall_4() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        let flows_tmp = tempfile::TempDir::new().unwrap();
        let _flows_guard = EnvGuard::set("DARKMUX_FLOWS_DIR", flows_tmp.path());
        write_echo_fixture(crew_tmp.path(), "echo-fixture", "fixture output");

        let router = |_msg: &str| -> Result<String> {
            Ok("```json\n{\"refuse\": \"that's outside the scope of mission comms\"}\n```".to_string())
        };
        let (mut writer, mut reader) = spawn_test_agent(router);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        send_prompt(&mut writer, &session_id, "what's the weather like on mars?").await;

        let refusal = recv_json(&mut reader).await;
        let text = chunk_text(&refusal);
        assert!(text.contains("that's outside the scope of mission comms"), "the reason is rendered verbatim: {text}");
        assert!(text.contains("echo-fixture"), "the live command listing follows the reason: {text}");

        let final_response = recv_json(&mut reader).await;
        assert_end_turn(&final_response);

        let day = darkmux_flow::day_utc_now();
        let flow_path = flows_tmp.path().join(format!("{day}.jsonl"));
        let flow_contents = std::fs::read_to_string(&flow_path).expect("wall 4's flow record file must exist");
        assert!(flow_contents.contains("\"action\":\"radio.route\""), "{flow_contents}");
        assert!(flow_contents.contains("\"decision\":\"refuse\""), "{flow_contents}");
        assert!(
            flow_contents.contains("that's outside the scope of mission comms"),
            "{flow_contents}"
        );
    }

    /// (#1698 Packet B — the mode bit's own investigation, confirmed
    /// empirically) A leading-slash invocation is LAW: it must resolve and
    /// execute exactly as before Packet B, and it must NEVER consult the
    /// router at all — the canned closure panics if called, so any
    /// invocation would fail the test loudly rather than silently passing.
    #[tokio::test]
    #[serial_test::serial]
    async fn slash_invocation_never_calls_the_router() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        write_echo_fixture(crew_tmp.path(), "echo-fixture", "fixture output");

        let router = |_msg: &str| -> Result<String> {
            panic!("the slash-command path must NEVER invoke the router — mode bit violation");
        };
        let (mut writer, mut reader) = spawn_test_agent(router);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        send_prompt(&mut writer, &session_id, "/echo-fixture").await;

        // No provenance chunk for the slash path (unchanged from pre-#1698
        // Packet B) — the FIRST notification is the command's own output.
        let output = recv_json(&mut reader).await;
        assert_eq!(chunk_text(&output), "fixture output");

        let final_response = recv_json(&mut reader).await;
        assert_end_turn(&final_response);
    }

    /// (#1698 Packet B — "bare-word invocation is retired") The retirement
    /// itself, at the ACP wire level: text with NO leading slash that
    /// happens to spell an advertised command's id EXACTLY must still go
    /// through the router — never fire the command by pattern-match alone.
    /// Proven by counting router invocations (must be exactly one) AND by
    /// configuring the canned router to REFUSE, so a bare-word bypass would
    /// show up as the command's OWN output arriving instead of a refusal.
    #[tokio::test]
    #[serial_test::serial]
    async fn bare_word_matching_a_command_name_does_not_fire_it_directly() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        write_echo_fixture(crew_tmp.path(), "echo-fixture", "fixture output");

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_for_router = call_count.clone();
        let router = move |_msg: &str| -> Result<String> {
            call_count_for_router.fetch_add(1, AtomicOrdering::SeqCst);
            Ok("```json\n{\"refuse\": \"ambiguous — bare word, no slash\"}\n```".to_string())
        };
        let (mut writer, mut reader) = spawn_test_agent(router);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        // Bare — no leading slash — and spells the fixture's OWN advertised
        // command id exactly. Pre-#1698-Packet-B `parse_command` would have
        // matched this as a bare-word command invocation.
        send_prompt(&mut writer, &session_id, "echo-fixture").await;

        let refusal = recv_json(&mut reader).await;
        assert!(
            chunk_text(&refusal).contains("ambiguous"),
            "a bare word must be classified by the router, never pattern-matched into a direct \
             execution: {}",
            chunk_text(&refusal)
        );

        let final_response = recv_json(&mut reader).await;
        assert_end_turn(&final_response);

        assert_eq!(
            call_count.load(AtomicOrdering::SeqCst),
            1,
            "the router must be consulted exactly once for bare no-slash text that spells a \
             command name — proving bare-word invocation is retired, not just usually refused"
        );
    }

    /// The inverted case for empty/whitespace text (red-prove discipline,
    /// and the issue's own "Empty/whitespace unchanged" requirement):
    /// blank text must render the plain "not a command" listing WITHOUT
    /// ever invoking the router (proving the no-slash channel's empty
    /// short-circuit still lives in `serve()` itself, not just in
    /// `radio::route`'s own defense-in-depth check).
    #[tokio::test]
    #[serial_test::serial]
    async fn empty_text_never_invokes_the_router() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        write_echo_fixture(crew_tmp.path(), "echo-fixture", "fixture output");

        let router = |_msg: &str| -> Result<String> {
            panic!("empty/whitespace text must never reach the router");
        };
        let (mut writer, mut reader) = spawn_test_agent(router);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        send_prompt(&mut writer, &session_id, "   ").await;

        let not_a_command = recv_json(&mut reader).await;
        assert!(
            chunk_text(&not_a_command).contains("doesn't recognize that as a command"),
            "{}",
            chunk_text(&not_a_command)
        );

        let final_response = recv_json(&mut reader).await;
        assert_end_turn(&final_response);
    }
}
