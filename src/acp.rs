//! `darkmux acp`: darkmux as an ACP (Agent Client Protocol) agent over
//! stdio, so editors like Zed drive the advertised catalog and radio's two
//! seats from their agent panel. Started as the #1388 spike; shipped through
//! the #1698 packets. The list below is the spike's original debts, each
//! marked as it was paid.
//!
//! ## Spike-era notes (historical)
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
//!   file's generic [`run_launch_command`] through that SAME routing path
//!   (rather than a hand-rolled string match or a bespoke `run_review`
//!   function — #2310 P4d deleted the dedicated review launcher, and
//!   `run_launch_command` synthesizes the diff/workspace inputs review
//!   needs via `acp_panel::synthesize_diff_launch_inputs` before spawning
//!   the same `mission launch <config_id>` subprocess every other panel
//!   command uses).
//! - (#2310 P4d — RESOLVED) Review-stage progress used to be recognized by
//!   pattern-matching known substrings out of the review subprocess's own
//!   stderr (`REVIEW_STAGES`/`recognize_stage`, since deleted along with
//!   the bespoke review launcher). `run_launch_command` has no stage
//!   concept at all now — it reports "launching…" then the final
//!   stdout/stderr, same as any other mission-launch panel command.
//! - (#1684 remainder — RESOLVED) Session state (the cwd per ACP session)
//!   used to live in an in-memory map that was never pruned. `session/close`
//!   is now advertised (`SessionCapabilities.close`) and handled: it aborts
//!   any in-flight command for the session (the SAME [`InFlight`]
//!   abort-handle registry `session/cancel` drives, below) and removes the
//!   session's `sessions` entry — that request is the only signal ACP gives
//!   an agent that a Zed thread is genuinely gone (there is no
//!   disconnect/drop notification at the protocol level). A client that
//!   never sends it (or predates the capability) still gets the
//!   process-level backstop already in place: `idle_self_exit_loop` (#1698
//!   Packet B2 scope G2) exits the whole process once nothing has been in
//!   flight for `acp_idle_exit_minutes` — but see the #1781 fix directly
//!   below, which is what actually makes that backstop safe to leave
//!   running underneath a real editor session.
//! - (#1781 — RESOLVED) The backstop above used to be unconditional: it
//!   fired purely off elapsed idle time and the `in_flight` command count,
//!   with no regard for whether a client still had a session open. A Zed
//!   panel left idle (no prompt sent) for `acp_idle_exit_minutes` — the
//!   ordinary case for a session someone is reading, not actively typing
//!   into — got its whole process killed out from under it, and there is no
//!   ACP reconnect: the panel stayed broken until the IDE itself restarted.
//!   `idle_self_exit_loop` is now two-tiered, keyed on a latch
//!   ([`IdleState`]) set the first time a client attaches a session
//!   (`session/new` or `session/load`) and NEVER cleared:
//!
//!   - Nothing ever attached — a spawned-but-unused process, the orphan
//!     case the backstop exists for. Still reclaimed at
//!     `acp_idle_exit_minutes`.
//!   - Something attached at some point — a real editor session. Reclaimed
//!     only after the week-scale hard ceiling ([`hard_idle_threshold`])
//!     with no byte from the client, so the disclosed orphan leak is
//!     BOUNDED rather than merely rare, without the bound ever landing
//!     inside ordinary use. Stated plainly, since it is the one case where
//!     this loop can still close a panel: a process whose client has been
//!     silent for a week of uptime is reclaimed, and a client that comes
//!     back after that gets the same `Incoming transport closed` — the
//!     ceiling's own doc explains why a week is where that trade was
//!     drawn.
//!
//!   The predicate is "has a session EVER attached", not "is one attached
//!   right now" (i.e. NOT `sessions.is_empty()`), because the latter
//!   reopens this same bug on a second path: `session/close` prunes the map
//!   (#1684, above), so a client that closes one thread and opens another
//!   minutes later empties the map in between while its transport and
//!   workspace stay live — and a currently-empty test exits under it,
//!   producing the reported `Incoming transport closed` verbatim. #1781
//!   names the ever-attached predicate itself, as its option 2. It also
//!   removes any dependence on whether a given client sends `session/close`
//!   at all.
//! - (#1684 remainder — RESOLVED) Cancellation is wired. `session/cancel`
//!   (`CancelNotification`) looks up the session's in-flight command in the
//!   [`InFlight`] registry and calls `AbortHandle::abort()` on it. The
//!   command execution itself now runs as its OWN `tokio::spawn`'d task
//!   (registered in that map for the duration of `session/prompt`'s
//!   `cx.spawn`'d closure from Packet 2) rather than directly inline in that
//!   closure — `cx.spawn` alone never hands back an abort handle, so
//!   cancellation needed a genuinely abortable task underneath it. Because
//!   [`run_launch_command`]'s subprocess `Command`s set
//!   `kill_on_drop(true)`, aborting the task — which drops the `Child`
//!   mid-`.wait()`/`.output()` — sends the OS process a real kill rather
//!   than orphaning it (the exact defect this packet's own audit named: an
//!   aborted ACP-side future used to leave the `darkmux mission launch`
//!   child running with nothing left to stop it). On cancellation the
//!   `session/prompt` response now carries `StopReason::Cancelled`, per
//!   spec.
//!
//!   **"No OS subprocess to leak" is true ONLY for the no-slash channel's
//!   router/answerer dispatch** — that path is a plain synchronous model
//!   call (`crate::radio::dispatch_router_call` / the answering seat), run
//!   on `tokio::task::spawn_blocking` because it's blocking, not because it
//!   shells out; there is genuinely no `Child` anywhere on that path. It is
//!   FALSE for [`run_ephemeral_command`]: `acp_panel::run_ephemeral`
//!   executes `procedural.shell` steps via
//!   `std::process::Command::output()` — a real OS subprocess, spawned on
//!   that SAME `spawn_blocking` thread, with no `kill_on_drop` and nothing
//!   holding a `Child` handle once that thread starts running (a prior
//!   version of this note claimed the ephemeral path had no subprocess to
//!   leak either — corrected, #1777 merge gate). Concretely: an operator
//!   runs `/pr-merge`, approves the sign-off dialog, then hits Zed's stop
//!   button while `gh pr merge` is running — aborting the outer task
//!   resolves the ACP wire with `StopReason::Cancelled` right away, but
//!   `spawn_blocking`'s closure cannot be preempted mid-call, so the merge
//!   keeps running to completion on its own thread, unkillable from here,
//!   with GitHub's own state as the only record it happened.
//!
//!   What IS fixed (#1777 merge gate, MUST FIX 1 tier 2): the closure's
//!   eventual result is no longer silently thrown away just because the
//!   task awaiting it got aborted. `run_ephemeral_command` wraps its
//!   `spawn_blocking` `JoinHandle` in [`EphemeralJoinGuard`], which — the
//!   instant the guard itself gets dropped WITHOUT `join` having completed
//!   (exactly the abort case) — hands the still-live handle to a fresh,
//!   UNTRACKED `tokio::spawn` that `session/cancel`/`session/close` can
//!   never reach (it's never registered in [`InFlight`]), so it survives
//!   the very abort that killed its parent. That detached task posts a
//!   `"completed after cancellation: ..."` chunk once the blocking work
//!   actually finishes, so a verb that DID execute (like the merge above)
//!   still leaves evidence in the transcript even though the cancel could
//!   not stop it. Genuinely making `procedural.shell` killable (running it
//!   via `tokio::process` with `kill_on_drop` instead of
//!   `std::process::Command::output()`) is the real fix, but it needs
//!   `StepKind::run` — the trait every builtin step kind implements, not
//!   just this one — to grow an async or cancellation-aware shape, which
//!   ripples well past this file; tracked as follow-up rather than forced
//!   into this packet.
//!
//!   Two more honestly-named gaps in the cancellation story, both INHERENT
//!   to killing a process rather than bugs in how it's wired (#1777 merge
//!   gate, CONSIDER items). (1) A `session/cancel` that races the outer
//!   `cx.spawn`'d task's very first poll — arriving before
//!   [`run_cancellable`] has inserted its own abort handle into
//!   [`InFlight`] — used to be a silently-lost no-op that let the command
//!   run to completion reporting `EndTurn` as if nothing happened;
//!   `InFlight` now stores a `Cancelled` tombstone (`InFlightSlot`) in that
//!   window, so the command aborts itself the instant it registers instead
//!   of racing ahead uncancelled. (2) A `kill_on_drop`'d SIGKILL has no
//!   finalize step — a cancelled `run_launch_command` mission
//!   is left permanently `Active` (`darkmux mission status` will flag it;
//!   a manual `mission abort` reconciles it), and the temp diff file
//!   `run_launch_command` writes is never cleaned up (`tokio::fs::remove_file`
//!   sits AFTER `child.wait().await`, a line a cancel never reaches).
//!   Neither is fixable by anything short of a completion-independent
//!   cleanup path; named here so a stop-button user isn't surprised by
//!   drift accumulating in `mission status` or stray files under the
//!   workspace.
//! - (#2476 — RESOLVED, revised in review round 2) This file had NO OS
//!   signal handling at all — a SIGINT/SIGTERM to `darkmux acp` itself (as
//!   opposed to an ACP-protocol `session/cancel`, covered above) killed
//!   the process by default disposition and orphaned whatever
//!   curl/subprocess child was in flight. `run()` now spawns
//!   [`host_shutdown_reap_loop`], which waits on `tokio::signal` (NOT
//!   `launch_guard::arm()`/raw `libc::signal()` — see
//!   [`wait_for_host_shutdown_signal_ready`]'s own doc for why that would be
//!   the wrong tool for a long-lived host) and, on a real signal, runs
//!   [`reap_on_host_shutdown`] before exiting. The `mission launch`
//!   subprocess [`run_launch_command`] spawns is registered via
//!   [`spawn_registered`] — the RAII guard [`SpawnedLaunchChild`] that
//!   function uses closes the one child this file spawns directly that
//!   `child_registry` didn't already cover through `darkmux-crew`'s own
//!   dispatch machinery, and — unlike the plain register/deregister pair
//!   the first cut of this fix used — deregisters correctly even when
//!   `session/cancel`/`session/close` abort the awaiting task mid-wait
//!   (an `AbortHandle::abort()` drops the future AT its suspended await
//!   point, skipping any deregister statement placed AFTER it; RAII
//!   `Drop` runs regardless). `reap_on_host_shutdown` does NOT reach the
//!   `mission launch` child with the same blanket SIGKILL it sends every
//!   other registered dispatch child: that subprocess holds its own
//!   `LaunchFinalizeGuard` and its own registered docker/curl
//!   grandchildren (unreachable from this process's registry), so
//!   SIGKILLing it races the finalize and orphans those grandchildren —
//!   #2476's own failure, one process down. It gets `radio_cli.rs`'s
//!   `forward_signal_and_wait` treatment instead: SIGTERM, a bounded
//!   wait ([`LAUNCH_CHILD_SHUTDOWN_GRACE`]), and — if it hasn't finished —
//!   left running rather than forced, exactly as that module's own doc
//!   explains for why a force-kill there was measured to make things
//!   worse, not better.
//!
//!   **What "RESOLVED" above does NOT cover (review round 2, CONSIDER
//!   6) — named explicitly rather than left implicit.** This entry only
//!   ever closed the SIGNAL gap. Three other ways this process's
//!   `run()` future can end are still uncovered by
//!   [`reap_on_host_shutdown`]/[`spawn_registered`]'s RAII guard, and a
//!   `mission launch` child in flight when any of them fires gets no
//!   grace at all — its `Child` simply drops, and `kill_on_drop(true)`
//!   sends it a bare SIGKILL:
//!   1. **`serve()` returning on client EOF** (the editor quits) —
//!      plausibly the single MOST common way this process ends in
//!      practice, and the one this doc previously left unnamed. No
//!      reap, no `mark_interrupted`, no grace.
//!   2. **`idle_self_exit_loop`'s process-level backstop** (#1698 Packet
//!      B2 scope G2, #1781) — the same SIGKILL-on-drop applies; the
//!      backstop's own `in_flight` count does not track the detached
//!      ephemeral-join task from the #1777 fix above, so it is not even
//!      a perfect proxy for "nothing is running."
//!   3. **A task panic** unwinding past the point where a `Child` is
//!      held.
//!
//!   Closing these needs a completion-independent cleanup path (the
//!   SAME gap this doc's #1684-remainder entry above already names for
//!   `kill_on_drop`'s SIGKILL having no finalize step), not more signal
//!   handling — tracked as follow-up, not forced into this fix's scope.
//! - The `case_id` passed to the review mission is derived from the diff's
//!   content hash + the cwd's directory name (see [`derive_case_id`]) —
//!   deterministic (no `Date`/random per the task brief) but not
//!   collision-proof across very different diffs that happen to hash the
//!   same 8 hex chars (astronomically unlikely; not worth guarding for a
//!   spike).
//! - (historical — the spike's bundler step is gone) Bundler routing used
//!   to be extension-sniffing on the diff (`choose_bundler`): TypeScript
//!   present → the built-in bundler; else `.edge` present → the
//!   operator's `darkmux-bundler-edge` plugin. The shipped `review`
//!   pipeline (plan → review → summarize → create-mods → deliver) takes
//!   no bundler input at all; `--bundler` survives only on `lab eval` /
//!   `lab review-bench`.
//! - (#1684 remainder — RESOLVED) The review subprocess path's stderr-draining loop (since folded into `run_launch_command`) used
//!   to forward every non-JSON, non-blank line straight into the chat as an
//!   agent chunk — including darkmux-flow's own sink-init diagnostics
//!   (`crates/darkmux-flow/src/lib.rs::build_default_sink`), which print
//!   UNCONDITIONALLY on stderr the first time any process touches the flow
//!   crate, i.e. every `mission launch` subprocess this file spawns.
//!   Observed live leaking into the Zed panel ("flow: Redis sink enabled —
//!   ... composed via TeeSink"). [`forwardable_chunk_text`] now drops any
//!   line starting with the flow crate's own `"flow: "` prefix — narrowly,
//!   not a broad heuristic — before it ever reaches the chat.
//!
//!   **Filter the chat, never the record (#1777 merge gate, MUST FIX 2).**
//!   The same `"flow: "` prefix is also how the flow crate spells its own
//!   DEGRADED-mode warnings — e.g. `"flow: Redis sink construction failed
//!   (...); continuing without it."` when a Redis password has rotted
//!   (`build_default_sink`, `crates/darkmux-flow/src/lib.rs`). A blanket
//!   drop made that warning invisible everywhere: every `/review`
//!   subprocess prints it, the filter ate it, and nothing on this
//!   process's own stderr said so either — a dark fleet stream with no
//!   diagnostic anywhere. `forwardable_chunk_text` now re-emits every line
//!   it drops (chunk-suppressed or not) as `[darkmux-acp] subprocess:
//!   <line>` on this process's OWN stderr (Zed surfaces that in its logs
//!   panel, per this file's own "why stdout is off-limits" convention
//!   above), so a degraded sink is never silently invisible — it's just
//!   not narrated in the chat transcript.
//!
//! ## Why stdout is off-limits
//!
//! ACP's wire transport IS stdout: `AcpStdio::new()` below wires the
//! JSON-RPC connection directly to this process's stdin/stdout
//! (`agent_client_protocol::Stdio`, aliased here to avoid colliding with
//! `std::process::Stdio`). A launched mission prints its own result to
//! STDOUT (`deliver.github_review` emits the rendered payload there when
//! `emit` is `-`), so this file NEVER runs a mission in-process — it always
//! shells out to a SUBPROCESS (the current executable, re-invoked as
//! `mission launch <id>`) with stdout/stderr captured as pipes, and never
//! writes anything but the ACP JSON-RPC stream to this process's own
//! stdout. Anything this file wants to log for its own debugging goes to
//! STDERR only (`[darkmux-acp]`-prefixed, matching the existing
//! `[darkmux-liveness]` convention) — Zed surfaces an agent's stderr in its
//! logs panel.

use agent_client_protocol::schema::v1::{
    AgentCapabilities, AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, CancelNotification,
    CloseSessionRequest, CloseSessionResponse, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionCapabilities, SessionCloseCapabilities,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectOption,
    SessionId, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    StopReason, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, UnstructuredCommandInput,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Stdio as AcpStdio};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio as ProcStdio;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::process::Command;

/// Per-session state: the `cwd` the client handed us in `session/new` (or
/// `session/load`), the session's artifact shelf (#1698 Packet B2, scope C
/// — the answering seat's own recent-history grounding source), and the
/// session's config-option overrides (scope F — the "radio host" / "humor"
/// pickers). Keyed by the session id we minted (or, for a loaded session,
/// the id the client asked to resume).
#[derive(Clone, Default)]
struct SessionState {
    cwd: PathBuf,
    shelf: crate::radio_answer::ArtifactShelf,
    overrides: crate::radio_answer::AnswererOverrides,
}

type Sessions = Arc<Mutex<HashMap<SessionId, SessionState>>>;

/// (#1684 remainder) The abort-handle registry `session/cancel` and
/// `session/close` both drive: keyed by session id, holding an
/// [`InFlightSlot`] for whatever command that session currently has
/// running (inserted by [`run_cancellable`] for the duration of one
/// `session/prompt`, removed when that command finishes on its own). A
/// session with nothing in flight simply has no entry — looking one up is
/// always a `remove`-and-check, never a panic on absence.
type InFlight = Arc<Mutex<HashMap<SessionId, InFlightSlot>>>;

/// One session's entry in [`InFlight`] — either a genuinely running
/// command's abort handle, or a `Cancelled` TOMBSTONE (#1777 merge gate,
/// CONSIDER — the "lost-cancel race").
///
/// The race: `PromptRequest`'s handler returns as soon as `cx.spawn`
/// SCHEDULES its task, not once that task actually starts running. On the
/// multi-thread runtime, `session/cancel`'s notification can therefore be
/// processed — and find `InFlight` empty for that session, since
/// [`run_cancellable`] hasn't reached its own insert yet — before the
/// command's first poll ever happens. Before this tombstone existed, that
/// window turned a genuine cancel into a silently logged no-op, and the
/// mission ran to completion reporting `EndTurn` as if nothing had been
/// asked of it. Recording `Cancelled` in that same window means
/// `run_cancellable`'s own insert attempt finds it and aborts immediately
/// instead of registering a handle nobody will ever call `abort()` on.
enum InFlightSlot {
    Running(tokio::task::AbortHandle),
    Cancelled,
}

/// `session/set_config_option`'s `config_id` for the "radio host" picker
/// (#1698 Packet B2, scope F) — selects the answering seat's profile.
const RADIO_HOST_CONFIG_ID: &str = "radio-host";
/// `session/set_config_option`'s `config_id` for the "humor" picker.
const RADIO_HUMOR_CONFIG_ID: &str = "humor";
/// The synthetic "use the configured default" choice on the radio-host
/// picker — selecting it CLEARS the session override rather than pinning a
/// literal profile named `"__default__"` (no such profile needs to exist).
const RADIO_HOST_DEFAULT_CHOICE: &str = "__default__";

/// Build the session config-option list reflecting `overrides`'s CURRENT
/// state — the same shape `session/new`'s `NewSessionResponse.config_options`
/// advertises and `session/set_config_option`'s response echoes back after
/// a change (#1698 Packet B2, scope F). Read-only — no dispatch, no I/O
/// beyond the registry read `radio_answer::available_profile_names` already
/// does.
fn build_session_config_options(overrides: &crate::radio_answer::AnswererOverrides) -> Vec<SessionConfigOption> {
    let mut host_choices: Vec<SessionConfigSelectOption> =
        vec![SessionConfigSelectOption::new(RADIO_HOST_DEFAULT_CHOICE, "Use configured default")];
    host_choices.extend(
        crate::radio_answer::available_profile_names()
            .into_iter()
            .map(|name| SessionConfigSelectOption::new(name.clone(), name)),
    );
    let host_current = overrides.profile_name.clone().unwrap_or_else(|| RADIO_HOST_DEFAULT_CHOICE.to_string());
    let radio_host = SessionConfigOption::select(RADIO_HOST_CONFIG_ID, "Radio host", host_current, host_choices)
        .description(Some(
            "Which profile answers grounded questions in the radio channel (the panel's no-slash chat)."
                .to_string(),
        ))
        .category(Some(SessionConfigOptionCategory::Model));

    let humor_choices: Vec<SessionConfigSelectOption> = crate::radio_answer::HUMOR_PRESETS
        .iter()
        .map(|h| SessionConfigSelectOption::new(h.to_string(), format!("{h}%")))
        .collect();
    let humor_current = overrides
        .humor
        .unwrap_or_else(darkmux_types::config_access::radio_humor)
        .to_string();
    let humor = SessionConfigOption::select(RADIO_HUMOR_CONFIG_ID, "Radio humor", humor_current, humor_choices)
        .description(Some("How much wit RADIO's persona spends versus plain answers.".to_string()))
        .category(Some(SessionConfigOptionCategory::ModelConfig));

    vec![radio_host, humor]
}

/// Send the `AvailableCommandsUpdate` notification advertising every
/// registry mission config that declares a `panel` block (#1684) — the
/// SAME resolution `darkmux mission launch` / `mission status` use.
///
/// Shared by `session/new` and `session/load` (#1698 Packet B2 gate): a
/// resumed session needs the menu as much as a fresh one, and one copy of
/// this means the two can't drift. Callers MUST have already responded to
/// the request — a `session/update` that reaches the client before the
/// response names a session id the client doesn't know yet, and Zed drops
/// exactly that update (observed live: "Available commands for darkmux:
/// none").
fn advertise_panel_commands(
    cx: &ConnectionTo<Client>,
    session_id: SessionId,
    origin: &str,
) -> Result<(), agent_client_protocol::Error> {
    let panel_commands = crate::acp_panel::list_panel_commands();
    eprintln!(
        "[darkmux-acp] {origin}: advertising {} panel command(s): {}",
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
}

/// Apply one `session/set_config_option` request to `overrides` in place.
/// Unrecognized `config_id`s and unrecognized values are no-ops (the
/// response still echoes the CURRENT — unchanged — option list, never a
/// protocol error, for the same "never bounce an error across the
/// boundary for something we don't support yet" reason `session/prompt`'s
/// unrecognized-command path already follows).
fn apply_config_option(overrides: &mut crate::radio_answer::AnswererOverrides, config_id: &str, value: &SessionConfigOptionValue) {
    let Some(value_id) = value.as_value_id() else { return };
    let raw = value_id.0.as_ref();
    match config_id {
        RADIO_HOST_CONFIG_ID => {
            overrides.profile_name = (raw != RADIO_HOST_DEFAULT_CHOICE).then(|| raw.to_string());
        }
        RADIO_HUMOR_CONFIG_ID => {
            if let Ok(n) = raw.parse::<u8>() {
                overrides.humor = Some(n.min(100));
            }
        }
        _ => {}
    }
}

/// (#1781) Everything [`idle_self_exit_loop`]'s decision reads, carried as
/// ONE shared value: the never-cleared "a client has attached a session to
/// this process" latch, the last-activity mark, and the count of commands
/// currently executing.
///
/// **Bundled deliberately, not for tidiness.** [`should_idle_exit`] takes
/// `&IdleState`, so the loop's call site cannot hand it a hand-written
/// stand-in for any of the three. The shape this replaced took a bare
/// `bool` for the session signal, and a review proved the whole fix could
/// be reverted by passing a literal `true` at the one call site the tests
/// never reached — `cargo build`, `cargo clippy -- -D warnings` and the
/// whole acp suite stayed green. Typing the state does not make that revert
/// impossible (a fresh `IdleState::new()` is unattached with
/// `last_activity: 0`, which is the same literal wearing a constructor); it
/// makes it CONSPICUOUS, and what actually catches it is
/// [`idle_self_exit_loop_with`] — the seam that lets a wire test run the
/// real loop against the real state a served connection is using.
///
/// **Time here is MONOTONIC** (seconds since `started`), never wall clock.
/// The idleness this meters is "this process has been up with nothing to
/// do", and on macOS `Instant` does not advance while the system is
/// suspended — so a laptop closed over a weekend accrues no idleness. That
/// only started mattering once the hard ceiling below could actually fire:
/// on a wall clock, a suspend longer than the ceiling would kill an
/// attached panel the moment the machine woke, which is #1781's own symptom
/// with a longer fuse. (Should some platform's monotonic clock include
/// suspend, the result is merely the wall-clock behavior we'd have had
/// anyway — this choice can only be the more conservative of the two.)
///
/// **#2479 must not sweep this into `SystemTime`.** That issue proposes a
/// project-wide `Instant` → `SystemTime` migration; applied here it would
/// silently revert the paragraph above and reintroduce the wake-from-
/// suspend kill. The clock choice is per-deadline, not per-project: a
/// deadline metering THIS PROCESS's own activity wants a monotonic clock,
/// a deadline metering the outside world wants a wall clock. This one is
/// the former.
struct IdleState {
    started: std::time::Instant,
    /// Monotonic seconds since `started` at the last observed client
    /// traffic — see [`IdleState::record_activity`].
    last_activity: AtomicU64,
    /// Commands currently executing (`session/prompt`'s two spawn arms).
    in_flight: AtomicI64,
    /// (#1781) Set the first time a client attaches a session
    /// (`session/new` or `session/load`) and **never cleared** — see
    /// [`should_idle_exit`] for why "has one ever attached" is the
    /// predicate and "is one attached right now" is not.
    session_ever_attached: AtomicBool,
}

impl IdleState {
    fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            last_activity: AtomicU64::new(0),
            in_flight: AtomicI64::new(0),
            session_ever_attached: AtomicBool::new(false),
        }
    }

    /// Monotonic seconds since this state was created — the clock
    /// [`should_idle_exit`]'s `now_secs` is measured on.
    fn elapsed_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Record client traffic. Every request/notification the client sends
    /// counts, including the ones that are not prompts — `session/close`
    /// and `session/cancel` are bytes crossing the transport just as much
    /// as a prompt is, and under a hard ceiling that distinction decides
    /// when the ceiling starts counting.
    fn record_activity(&self) {
        self.last_activity.store(self.elapsed_secs(), Ordering::SeqCst);
    }

    /// (#1781) Latch the "a client attached a session" fact — `session/new`
    /// and `session/load` are its only callers, and nothing ever clears it.
    /// Also records activity, since attaching is itself client traffic.
    fn record_session_attached(&self) {
        self.session_ever_attached.store(true, Ordering::SeqCst);
        self.record_activity();
    }

    fn command_started(&self) {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
    }

    /// Stamps activity on COMPLETION before dropping the count — otherwise
    /// a long-running command (a multi-minute `/review`) drops `in_flight`
    /// to 0 the instant it finishes while `last_activity` still holds the
    /// RECEIPT mark from minutes ago, and the very next tick could see a
    /// stale idle window while the operator is still reading the result.
    fn command_finished(&self) {
        self.record_activity();
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// (#1781) How many times the configured idle window an ATTACHED process
/// gets before the backstop reclaims it anyway — see
/// [`hard_idle_threshold`].
const HARD_IDLE_MULTIPLIER: u64 = 48;

/// (#1781) Floor under the hard ceiling — see [`hard_idle_threshold`] for
/// why it is a WEEK and not a day.
const HARD_IDLE_FLOOR_SECONDS: u64 = 7 * 24 * 60 * 60;

/// (#1781) The ceiling an ATTACHED process is measured against: 48× the
/// configured soft window, never less than a week of *uptime* with no byte
/// from the client.
///
/// **Why a week, when a day sounds like plenty.** The two costs this trades
/// between are wildly asymmetric. An `darkmux acp` process that outlives its
/// client holds no model weights — those live in LMStudio — so a leaked one
/// costs a few MB of RSS and nothing else. A ceiling that fires under a
/// panel someone is still using costs an IDE restart and IS the bug this
/// issue is about, since ACP has no reconnect. Bounding the cheap failure
/// must not risk the expensive one, and a day is inside ordinary use: an
/// always-on machine with an editor open across a long weekend reaches 24 h
/// of untouched uptime without anything unusual happening. A week does not
/// — by then the editor has almost certainly been restarted anyway — while
/// still turning "leaks forever" into "leaks for at most a week", which is
/// the whole point of having a ceiling.
///
/// **Why a multiplier on top of the floor.** An operator who lengthens
/// `acp_idle_exit_minutes` past ~3.5 h is stating a longer patience for
/// this process; the ceiling scales with that rather than being pinned
/// behind their back. Below that the floor governs — which is also what
/// keeps an operator who SHORTENS the window (the knob's documented job is
/// reclaiming spawned-but-unused processes faster) from unknowingly arming
/// a short kill on the panel they are reading.
fn hard_idle_threshold(idle_threshold_seconds: u64) -> u64 {
    idle_threshold_seconds.saturating_mul(HARD_IDLE_MULTIPLIER).max(HARD_IDLE_FLOOR_SECONDS)
}

/// (#1781) The whole decision [`idle_self_exit_loop`] acts on, as a pure
/// function of state it is HANDED — so it can be proven without going
/// anywhere near `std::process::exit`, and so the loop cannot drift away
/// from what the tests pin (see [`IdleState`]'s own note on the literal
/// that reverted the previous shape).
///
/// Two tiers, keyed on the never-cleared latch:
///
/// - **Nothing has ever attached** — a spawned-but-unused process, the
///   orphan case the backstop exists for. Reclaimed at the configured
///   window.
/// - **Something attached at some point** — a real editor session, whether
///   or not it is open right now. Reclaimed only at
///   [`hard_idle_threshold`].
///
/// The predicate is "has a session EVER attached", not "is one attached
/// now", because the latter reopens #1781 on a second path: `session/close`
/// removes the map entry, so a client that closes one thread and opens
/// another minutes later empties the map in between while its transport and
/// workspace stay live — and a "currently empty" test would exit under it,
/// producing the reported `Incoming transport closed` verbatim. #1781 names
/// this predicate itself, as its option 2 ("only fire when no session has
/// ever been created, which is the actual orphan case").
fn should_idle_exit(state: &IdleState, now_secs: u64, idle_threshold_seconds: u64) -> bool {
    if state.in_flight.load(Ordering::SeqCst) > 0 {
        return false;
    }
    let idle_for = now_secs.saturating_sub(state.last_activity.load(Ordering::SeqCst));
    let threshold = if state.session_ever_attached.load(Ordering::SeqCst) {
        hard_idle_threshold(idle_threshold_seconds)
    } else {
        idle_threshold_seconds
    };
    idle_for >= threshold
}

/// (#1698 Packet B2, scope G2; gated per #1781 — see [`should_idle_exit`])
/// Background idle self-exit loop, spawned once per `serve()` call. Checks
/// every 60s (a check-cadence far below any realistic idle threshold, so it
/// never meaningfully delays the exit); on a MINUTES-scale idle threshold
/// this coarseness is a non-issue. Exits the WHOLE PROCESS
/// (`std::process::exit(0)`) — never returns an error, never tears down the
/// connection gracefully first, because there is nothing left to tear down:
/// zero commands are running, by construction of the check itself, and
/// nothing has spoken to this process for at least a week if a session was
/// ever attached. `acp_idle_exit_minutes == 0` disables the loop entirely
/// (checked once, up front — not on every tick, so a `0` config never even
/// starts the sleep loop).
///
/// The loop reads ATOMICS only — no mutex, so no lock this task could find
/// poisoned by a panicking handler and then panic on itself, silently
/// disabling the backstop in a process whose job is being observable. That
/// failure mode is gone by construction rather than handled.
///
/// This function is deliberately nothing but config reading + wiring: the
/// whole decision loop lives in [`idle_self_exit_loop_with`], which a test
/// can run for real (see `the_real_loop_never_reclaims_an_attached_process`)
/// because both of the things that make this one untestable — the config
/// read and `std::process::exit` — are parameters there.
async fn idle_self_exit_loop(idle: Arc<IdleState>) {
    let idle_minutes = darkmux_types::config_access::acp_idle_exit_minutes();
    if idle_minutes == 0 {
        return;
    }
    idle_self_exit_loop_with(
        idle,
        idle_minutes.saturating_mul(60),
        std::time::Duration::from_secs(60),
        |message| {
            eprintln!("{message}");
            std::process::exit(0);
        },
    )
    .await;
}

/// (#1781) The idle self-exit loop itself, with its two untestable
/// dependencies lifted into parameters: the check `tick` (60s in
/// production, ~1ms under test, so a test never waits a minute to learn
/// what the loop decides) and `on_exit`, which production wires to
/// `eprintln!` + `std::process::exit(0)` and a test wires to a counter.
///
/// **This seam is the point, not a convenience.** With the loop's decision
/// reachable only through [`idle_self_exit_loop`], the one call site that
/// feeds [`should_idle_exit`] its state was unreachable by any test — and a
/// review proved the entire #1781 fix could be reverted there, by passing a
/// freshly-constructed `IdleState` (unattached, `last_activity: 0`, so
/// every process falls into the orphan tier), with `cargo build`, `cargo
/// clippy -- -D warnings` and the whole acp suite still green. Handing the
/// loop the REAL state a served connection is wired to, and running it, is
/// what makes that revert red.
///
/// Returns after `on_exit` so a test's callback doesn't spin; in production
/// `on_exit` diverges and this never returns.
async fn idle_self_exit_loop_with(
    idle: Arc<IdleState>,
    idle_seconds: u64,
    tick: std::time::Duration,
    on_exit: impl Fn(String),
) {
    loop {
        tokio::time::sleep(tick).await;
        let now_secs = idle.elapsed_secs();
        if should_idle_exit(&idle, now_secs, idle_seconds) {
            let idle_for = now_secs.saturating_sub(idle.last_activity.load(Ordering::SeqCst));
            let reason = if idle.session_ever_attached.load(Ordering::SeqCst) {
                format!(
                    "no client traffic since the last session, past the {}s hard ceiling",
                    hard_idle_threshold(idle_seconds)
                )
            } else {
                format!("no session ever attached, past the {idle_seconds}s configured window")
            };
            on_exit(format!(
                "[darkmux-acp] idle for {idle_for}s with zero commands in flight — {reason}; \
                 self-exiting"
            ));
            return;
        }
    }
}

/// The no-slash channel's routing dispatch (#1698 Packet B), injectable so
/// `serve()` can be driven in a test over an in-process transport with a
/// CANNED router — never a live model — while `run()`'s production call
/// site wires the real `crate::radio::dispatch_router_call`. `Arc<dyn Fn>`
/// (not `radio::ModelCall`'s borrowed `FnMut`) because this needs to be
/// cloned into MULTIPLE `'static` `async move` closures registered on the
/// connection builder, one per `session/prompt` call, outliving `serve`'s
/// own stack frame.
type RouterCall = Arc<dyn Fn(&str) -> Result<String> + Send + Sync>;

/// The ANSWERING seat's dispatch (#1698 Packet B2), injectable for the SAME
/// reason [`RouterCall`] is — `serve()` can be driven in a test with a
/// CANNED answerer, so a router refusal (which now routes to this seat
/// instead of rendering the bare reason) never touches a live model under
/// test either. Takes the fully-assembled user message (grounding + the
/// original text — see `radio_answer::build_answer_message`) plus the
/// session's config-option overrides; `run()`'s production call site wraps
/// `radio_answer::dispatch_answerer_call_with` with the surface pinned to
/// [`crate::radio::RadioSurface::Panel`] — ACP is the panel surface by
/// construction, so that argument is never a runtime choice here the way
/// it is in `radio_cli.rs` (#1861).
type AnswererCall = Arc<dyn Fn(&str, &crate::radio_answer::AnswererOverrides) -> Result<String> + Send + Sync>;

/// The DATA-BOUNDARY seam (#1698 Packet B2 gate): how much of this
/// machine's state may go into the answering seat's grounding bundle, given
/// the session's overrides. Production wires
/// `radio_answer::grounding_scope_for`, which resolves the seat's profile
/// and answers `RemoteSafe` when it targets a remote endpoint.
///
/// Injectable for the same reason the model call is: resolving it reads the
/// profile registry off disk, so a pipe test asserting that a shelf entry
/// reaches the dispatch would otherwise depend on the HOST's
/// `default_profile` — and would fail, correctly but uselessly, on a
/// remote-only machine (the Studio has no local models). The boundary
/// itself is unit-tested directly in `radio_answer`; this seam keeps the
/// wire tests testing the wire.
type ScopeCall = Arc<dyn Fn(&crate::radio_answer::AnswererOverrides) -> crate::radio_answer::GroundingScope + Send + Sync>;

/// The answering seat's two injectable seams, carried together: the model
/// call, and the data-boundary decision that governs what may be put IN
/// that call (#1698 Packet B2 gate). One struct rather than two positional
/// parameters because they are never meaningfully separable — a caller
/// holding the ability to dispatch the seat must also hold the rule about
/// what it may be handed.
#[derive(Clone)]
struct AnsweringSeat {
    call: AnswererCall,
    scope: ScopeCall,
}

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
    // (#1861) ACP is the panel surface by construction — pinned here, not
    // threaded as a runtime choice the way `radio_cli.rs` threads the CLI
    // surface. Pinned by a test; see this file's test module.
    let answerer: AnswererCall = Arc::new(|m: &str, overrides: &crate::radio_answer::AnswererOverrides| {
        crate::radio_answer::dispatch_answerer_call_with(m, overrides, crate::radio::RadioSurface::Panel)
    });
    let scope: ScopeCall = Arc::new(crate::radio_answer::grounding_scope_for);
    rt.block_on(async {
        // (#2476) Reap-on-signal for the whole long-lived host — see
        // `host_shutdown_reap_loop`'s own doc for why this exits the
        // process rather than continuing to serve, and
        // `reap_on_host_shutdown`'s own doc (#2476 review round 2, MUST
        // FIX 2) for why it treats `mission launch` children differently
        // from every other registered dispatch child.
        tokio::spawn(host_shutdown_reap_loop(reap_on_host_shutdown));
        serve(router, AnsweringSeat { call: answerer, scope }, Arc::new(IdleState::new()), AcpStdio::new()).await
    })?;
    Ok(0)
}

/// (#2476 review round 2, MUST FIX 2) How long [`reap_on_host_shutdown`]
/// waits after forwarding SIGTERM to a still-running `mission launch`
/// child before it gives up watching it and proceeds to exit — mirrors
/// `radio_cli.rs`'s `FORWARD_SIGNAL_GRACE` and the same measured
/// reasoning: the child's `LaunchFinalizeGuard` write is `fsync`-bound,
/// not a fixed small cost, so no fixed number makes a force kill safe
/// (see `forward_signal_and_wait`'s own doc). This bounds only how long
/// THIS process stays alive watching before it exits anyway — never a
/// kill budget.
const LAUNCH_CHILD_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// (#2476 review round 2, MUST FIX 2) The production reap-on-signal
/// action [`run`] hands to [`host_shutdown_reap_loop`]. Split out (rather
/// than inlined in the closure, as before this fix) so its own doc has
/// somewhere to live.
///
/// **Why `mission launch` children get gentler treatment than every
/// other registered dispatch child.** SIGKILLing a `mission launch`
/// subprocess races its own `LaunchFinalizeGuard` (`launch_guard.rs`),
/// leaves the mission permanently `active` with no terminal record
/// (violating the dispatch-liveness contract every other signal-aware
/// launcher in this codebase honors), and orphans that child's OWN
/// registered docker/curl grandchildren — this process's registry
/// cannot reach them, because they live in the CHILD's own registry
/// entries, reaped only by ITS OWN signal handling, not this one. That is
/// #2476's own failure, one process down.
///
/// The fix mirrors `radio_cli.rs`'s `forward_signal_and_wait`: forward
/// SIGTERM to each registered launch child, wait — bounded — for it to
/// finalize on its own, and never escalate to SIGKILL if it doesn't
/// (`radio_cli.rs`'s own doc: "an earlier cut escalated to SIGKILL here
/// and was measured destroying a real finalize under ordinary machine
/// load"). Every OTHER registered child (the router/answerer curl
/// dispatches) holds no such state, so the blanket `kill_all_except`
/// sweep below is still the right tool for them — this function only
/// carves the launch children OUT of that sweep; it does not spare them
/// from being signaled at all, only from being FORCED.
fn reap_on_host_shutdown() {
    eprintln!("[darkmux-acp] shutdown signal received — reaping in-flight dispatch children");
    darkmux_types::interrupt::mark_interrupted();

    let launch_pids: std::collections::BTreeSet<u32> =
        LAUNCH_CHILDREN.lock().map(|s| s.clone()).unwrap_or_default();
    for pid in &launch_pids {
        if let Err(e) = darkmux_types::child_registry::kill_pid(*pid, darkmux_types::child_registry::SIGTERM) {
            if e.raw_os_error() != Some(darkmux_types::child_registry::ESRCH) {
                eprintln!(
                    "[darkmux-acp] could not forward the shutdown signal to `mission launch` (pid {pid}): {e}"
                );
            }
        }
    }
    if !launch_pids.is_empty() {
        let deadline = std::time::Instant::now() + LAUNCH_CHILD_SHUTDOWN_GRACE;
        loop {
            // (#2476 review round 2) `kill(pid, 0)` — the standard POSIX
            // existence probe, not a real signal — is what "still alive"
            // means here; `kill_pid` validates only the pid shape, not the
            // signal value, so passing `0` through it is safe.
            let still_alive =
                launch_pids.iter().any(|pid| darkmux_types::child_registry::kill_pid(*pid, 0).is_ok());
            if !still_alive {
                break;
            }
            if std::time::Instant::now() >= deadline {
                eprintln!(
                    "[darkmux-acp] `mission launch` (pid(s) {launch_pids:?}) still finishing after {}s \
                     — exiting now without forcing it down; `darkmux mission status` shows whether it \
                     finalized, `darkmux mission abort <id>` closes it if not.",
                    LAUNCH_CHILD_SHUTDOWN_GRACE.as_secs()
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    darkmux_types::child_registry::kill_all_except(darkmux_types::child_registry::SIGKILL, &launch_pids);
    std::process::exit(130);
}

/// `transport` is generic (#1698 Packet B test infrastructure) — production
/// (`run()` above) passes real `AcpStdio::new()`; pipe-level tests pass
/// `agent_client_protocol::ByteStreams::new(writer, reader)` over an
/// in-process `tokio::io::duplex`, so the SAME connection-handling code
/// this function builds runs in both, never a second test-only copy of the
/// handler chain.
///
/// `idle` is injectable for the same reason and by the same convention
/// (#1781): the latch's SET sites live inside these handler closures, where
/// no unit test can reach them, and a set-site that nothing observes is a
/// set-site a mutation can delete for free. Production (`run()` above)
/// hands in a fresh `IdleState`; the wire tests hand in one they keep a
/// handle on, so `session/new`/`session/load` actually latching it is an
/// asserted fact rather than an assumed one.
async fn serve(
    router_call: RouterCall,
    seat: AnsweringSeat,
    idle: Arc<IdleState>,
    transport: impl agent_client_protocol::ConnectTo<Agent> + 'static,
) -> Result<()> {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let next_session_ordinal = Arc::new(AtomicU64::new(1));

    // (#1698 Packet B2, scope G2 — idle self-exit; gated on the
    // ever-attached latch per #1781) `IdleState` carries the three things
    // the loop below decides on: activity (stamped by every handler that
    // sees client traffic), the in-flight command count, and the
    // never-cleared "a client attached a session here" latch. The loop
    // reclaims a spawned-but-unused process at `acp_idle_exit_minutes`
    // ("most swaps find no process running" — the issue's session-hygiene
    // addendum) and a once-attached one only at the week-scale hard ceiling.
    // See `IdleState`'s and `should_idle_exit`'s own docs. The state
    // itself is a parameter (see this function's own doc) so a wire test
    // can observe the latch these handlers set.
    tokio::spawn(idle_self_exit_loop(idle.clone()));

    // (#1684 remainder) The abort-handle registry `session/cancel` and
    // `session/close` both drive — see [`InFlight`]'s own doc. Distinct from
    // `in_flight` above: that's a bare COUNT (for the idle self-exit loop);
    // this is a per-session HANDLE (for actually tearing a command down).
    let in_flight_tasks: InFlight = Arc::new(Mutex::new(HashMap::new()));

    let sessions_for_new = sessions.clone();
    let ordinal_for_new = next_session_ordinal.clone();
    let sessions_for_prompt = sessions.clone();
    let router_for_prompt = router_call.clone();
    let seat_for_prompt = seat.clone();
    let sessions_for_load = sessions.clone();
    let sessions_for_config = sessions.clone();
    let sessions_for_close = sessions.clone();
    let idle_for_new = idle.clone();
    let idle_for_prompt = idle.clone();
    let idle_for_load = idle.clone();
    let idle_for_config = idle.clone();
    let idle_for_cancel = idle.clone();
    let idle_for_close = idle.clone();
    let in_flight_tasks_for_prompt = in_flight_tasks.clone();
    let in_flight_tasks_for_cancel = in_flight_tasks.clone();
    let in_flight_tasks_for_close = in_flight_tasks.clone();

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
                        .agent_capabilities(
                            AgentCapabilities::new()
                                // (#1698 Packet B2, scope G1) Advertise
                                // minimal `session/load` support — accept a
                                // resume, restore cwd, replay nothing (see
                                // the `LoadSessionRequest` handler below).
                                .load_session(true)
                                // (#1684 remainder — session hygiene)
                                // Advertise `session/close` — see the
                                // `CloseSessionRequest` handler below and
                                // the module doc's own note on why this is
                                // the map-pruning mechanism.
                                .session_capabilities(
                                    SessionCapabilities::new().close(SessionCloseCapabilities::new()),
                                ),
                        ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, cx: ConnectionTo<Client>| {
                // (#1781) Latches the ever-attached flag — from here on,
                // this process is only reclaimable at the hard ceiling.
                idle_for_new.record_session_attached();
                let ordinal = ordinal_for_new.fetch_add(1, Ordering::Relaxed);
                let session_id = SessionId::new(format!("darkmux-acp-{ordinal}"));
                let overrides = crate::radio_answer::AnswererOverrides::default();
                sessions_for_new.lock().expect("darkmux acp: sessions mutex poisoned").insert(
                    session_id.clone(),
                    SessionState { cwd: request.cwd.clone(), shelf: Default::default(), overrides: overrides.clone() },
                );

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
                //
                // (#1698 Packet B2, scope F) `config_options` advertises the
                // "radio host" + "humor" pickers per the vendored v1 schema
                // (`NewSessionResponse.config_options` / `SessionConfigOption`)
                // — whether Zed RENDERS them is unknown; see the PR body's
                // schema-finding section.
                responder.respond(
                    NewSessionResponse::new(session_id.clone())
                        .config_options(Some(build_session_config_options(&overrides))),
                )?;

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
                advertise_panel_commands(&cx, session_id, "session/new")
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, cx: ConnectionTo<Client>| {
                idle_for_prompt.record_activity();
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
                    let sessions_for_task = sessions_for_prompt.clone();
                    let idle_for_task = idle_for_prompt.clone();
                    let in_flight_tasks_for_task = in_flight_tasks_for_prompt.clone();
                    return cx.spawn(async move {
                        // (#1698 Packet B2, scope G2) Incremented HERE, as
                        // the future's own first action, not before
                        // `cx.spawn` — a `cx.spawn` call that itself returns
                        // `Err` (scheduling failure) never runs this body at
                        // all, so incrementing before the call would leak a
                        // count nothing ever decrements, disabling idle
                        // self-exit for the rest of the process's life.
                        idle_for_task.command_started();
                        // Robustness rule (see the task brief): NOTHING from
                        // here down may panic or propagate a hard error across
                        // the protocol boundary. A crashed-looking agent in
                        // Zed has no explanation; a chunk of error text does.
                        //
                        // (#1684 remainder — cancellation) `run_cancellable`
                        // runs the actual command as its own `tokio::spawn`'d
                        // task, registered in `in_flight_tasks` under this
                        // session id for the duration — the seam
                        // `session/cancel`/`session/close` abort into. See
                        // `InFlight`'s and `run_cancellable`'s own docs.
                        let work_session_id = session_id.clone();
                        let work_cx = cx_task.clone();
                        let work_sessions = sessions_for_task.clone();
                        let work_cwd = cwd.clone();
                        let work_args = args.clone();
                        let stop_reason = run_cancellable(
                            &in_flight_tasks_for_task,
                            session_id.clone(),
                            cx_task.clone(),
                            async move {
                                execute_route_plan(
                                    &work_session_id,
                                    plan,
                                    &work_args,
                                    &work_cwd,
                                    &work_cx,
                                    &work_sessions,
                                )
                                .await
                            },
                        )
                        .await;
                        // Stamps activity on COMPLETION before dropping
                        // the count, not just on receipt at the top of the
                        // handler — see `IdleState::command_finished`'s own
                        // doc for why receipt-only stamping is wrong.
                        idle_for_task.command_finished();

                        responder.respond(PromptResponse::new(stop_reason))
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
                let seat_for_task = seat_for_prompt.clone();
                let sessions_for_task = sessions_for_prompt.clone();
                let idle_for_task = idle_for_prompt.clone();
                let in_flight_tasks_for_task = in_flight_tasks_for_prompt.clone();
                cx.spawn(async move {
                    // (#1698 Packet B2, scope G2) See the slash-path arm's
                    // own comment on why this increments HERE, inside the
                    // future, rather than before `cx.spawn`.
                    idle_for_task.command_started();
                    // (#1684 remainder — cancellation) Same `run_cancellable`
                    // wrapping as the slash path above — see its own doc.
                    let work_session_id = session_id.clone();
                    let work_cx = cx_task.clone();
                    let work_sessions = sessions_for_task.clone();
                    let work_cwd = cwd.clone();
                    let work_text = text.clone();
                    let stop_reason = run_cancellable(
                        &in_flight_tasks_for_task,
                        session_id.clone(),
                        cx_task.clone(),
                        async move {
                            run_no_slash_route(
                                &work_session_id,
                                &work_text,
                                &work_cwd,
                                &work_cx,
                                router_for_task,
                                seat_for_task,
                                &work_sessions,
                            )
                            .await
                        },
                    )
                    .await;
                    // Stamped on completion — see
                    // `IdleState::command_finished`'s own doc on why
                    // receipt-only stamping is wrong.
                    idle_for_task.command_finished();
                    responder.respond(PromptResponse::new(stop_reason))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        // (#1698 Packet B2, scope G1) Minimal `session/load` — accept the
        // resume, restore `cwd`, replay NOTHING. Commands are stateless
        // views and the shelf's restart loss is already doctrine (#1684),
        // so an empty-history resume is fully functional: Zed keeps the
        // client-side visible transcript, and the FIRST prompt after resume
        // just re-derives the current catalog/config/board state, same as
        // any other prompt. `LoadSessionResponse::new()` (no modes, no
        // config_options) is a fully spec-conformant minimal response per
        // the vendored v1 schema (`Default`-derived, every field optional).
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, cx: ConnectionTo<Client>| {
                // (#1781) A resume attaches a session just as `session/new`
                // does — same latch, for the same reason.
                idle_for_load.record_session_attached();
                eprintln!(
                    "[darkmux-acp] session/load: {} cwd={}",
                    request.session_id,
                    request.cwd.display()
                );
                // (#1698 Packet B2 review finding) `and_modify` + `or_insert_with`,
                // NOT a blind `insert` — a session id this process ALREADY
                // holds live (the "Zed reconnects to a still-running
                // process" case, not just the "process restarted" case)
                // must keep its shelf + config-option overrides; only `cwd`
                // is refreshed from the request either way. A genuinely
                // unknown id (the restart case) gets a fresh empty
                // `SessionState` — replay nothing still holds for THAT case.
                let overrides = {
                    let mut guard =
                        sessions_for_load.lock().expect("darkmux acp: sessions mutex poisoned");
                    let state = guard
                        .entry(request.session_id.clone())
                        .and_modify(|s| s.cwd = request.cwd.clone())
                        .or_insert_with(|| SessionState {
                            cwd: request.cwd.clone(),
                            shelf: Default::default(),
                            overrides: Default::default(),
                        });
                    state.overrides.clone()
                };

                // (#1698 Packet B2 gate) Respond WITH the config-option
                // pickers, and re-advertise the command menu after — the
                // vendored v1 schema carries `config_options` on
                // `LoadSessionResponse` for exactly this ("initial session
                // configuration options"), and `session/new` follows its own
                // response with an `AvailableCommandsUpdate`. A bare
                // `LoadSessionResponse::new()` is spec-legal but leaves a
                // resumed thread with no pickers and possibly an empty slash
                // menu — which defeats this scope's whole purpose, since the
                // reason `session/load` exists here is to make binary swaps
                // and reconnects INVISIBLE. (Typed `/pr-list` still works
                // either way: `route_command` resolves against the registry,
                // never against the advertised list.)
                //
                // Same respond-FIRST ordering as `session/new`: a
                // notification naming a session id the client hasn't been
                // told about yet gets dropped (observed live, #1684).
                responder.respond(
                    LoadSessionResponse::new()
                        .config_options(Some(build_session_config_options(&overrides))),
                )?;
                advertise_panel_commands(&cx, request.session_id.clone(), "session/load")
            },
            agent_client_protocol::on_receive_request!(),
        )
        // (#1698 Packet B2, scope F) `session/set_config_option` — applies
        // the "radio host" / "humor" picker change to this session's
        // overrides and echoes the full updated option list back, per the
        // vendored v1 schema's `SetSessionConfigOptionResponse` contract.
        .on_receive_request(
            async move |request: SetSessionConfigOptionRequest, responder, _cx| {
                // (#1698 Packet B2 gate) Picker changes are ACTIVITY. Without
                // this, an operator reading results and adjusting the humor
                // or radio-host picker for longer than the idle window can
                // have the process exit under them — the one interaction
                // that proves someone is at the keyboard would be the one
                // interaction that doesn't count as being at the keyboard.
                idle_for_config.record_activity();
                // (#1698 Packet B2 review finding) `get_mut`, NOT
                // `entry(...).or_default()` — the wire-supplied session id
                // is untrusted input; materializing a `SessionState` for an
                // id this process never minted (via `session/new` or
                // `session/load`) would grow the map unboundedly from the
                // client AND leave `cwd` empty, which would later bypass
                // `session_cwd`'s `None` guard and route a prompt against
                // `cwd=""` instead of the clean "no working directory"
                // chunk. An unknown id is a no-op: nothing persists, and the
                // echoed list reflects the (unset) defaults — matching
                // `session_shelf_push`'s own no-op-on-unknown-session
                // convention.
                // (#1698 Packet B2 review finding) `build_session_config_options`
                // reads the profile REGISTRY off disk
                // (`radio_answer::available_profile_names`) — mutate the
                // session's overrides under the lock, clone them out, and
                // release the lock BEFORE that disk read, same "never do
                // I/O while holding the sessions mutex" shape `session/new`
                // already follows for its own advertised-command read.
                let overrides = {
                    let mut guard = sessions_for_config.lock().expect("darkmux acp: sessions mutex poisoned");
                    match guard.get_mut(&request.session_id) {
                        Some(state) => {
                            apply_config_option(&mut state.overrides, request.config_id.0.as_ref(), &request.value);
                            state.overrides.clone()
                        }
                        None => {
                            eprintln!(
                                "[darkmux-acp] session/set_config_option: unknown session {} — ignoring",
                                request.session_id
                            );
                            crate::radio_answer::AnswererOverrides::default()
                        }
                    }
                };
                let updated = build_session_config_options(&overrides);
                responder.respond(SetSessionConfigOptionResponse::new(updated))
            },
            agent_client_protocol::on_receive_request!(),
        )
        // (#1684 remainder — cancellation) Zed's stop button. Looks up the
        // session's in-flight command in `InFlight` and aborts it — see
        // `InFlight`'s own doc, `run_cancellable`, and the module doc's
        // "Cancellation is wired" note for the full mechanism, including why
        // aborting the task actually kills the OS subprocess rather than
        // orphaning it. A cancel for a session with nothing in flight (the
        // command already finished, or the id is unknown) records a
        // `Cancelled` tombstone instead of a bare no-op (#1777 merge gate,
        // CONSIDER — the lost-cancel race; see `InFlightSlot`'s own doc) —
        // `session/cancel` stays fire-and-forget by protocol design either
        // way, so there is still no response to fail even if it were an
        // error.
        .on_receive_notification(
            async move |cancel: CancelNotification, _cx| {
                // (#1781) A stop-button press is client traffic — somebody
                // is at the keyboard. Without this stamp the hard ceiling
                // would keep counting from before the cancel.
                idle_for_cancel.record_activity();
                let mut guard =
                    in_flight_tasks_for_cancel.lock().expect("darkmux acp: in-flight tasks mutex poisoned");
                match guard.remove(&cancel.session_id) {
                    Some(InFlightSlot::Running(handle)) => {
                        eprintln!(
                            "[darkmux-acp] session/cancel: {} — aborting the in-flight command",
                            cancel.session_id
                        );
                        drop(guard);
                        handle.abort();
                    }
                    Some(InFlightSlot::Cancelled) => {
                        // Already tombstoned by an earlier cancel that
                        // ALSO raced ahead of the command's own
                        // registration — restore the tombstone rather
                        // than losing it; still a no-op notification-wise.
                        guard.insert(cancel.session_id.clone(), InFlightSlot::Cancelled);
                        eprintln!(
                            "[darkmux-acp] session/cancel: {} — already tombstoned by an earlier \
                             cancel",
                            cancel.session_id
                        );
                    }
                    None => {
                        // (#1777 merge gate — lost-cancel race) Nothing
                        // registered YET, which is ambiguous on its own:
                        // either the command already finished (a genuine
                        // no-op) or it hasn't reached `run_cancellable`'s
                        // own insert yet (the race). Recording a tombstone
                        // costs nothing in the first case (nothing will
                        // ever consume it) and closes the race in the
                        // second.
                        guard.insert(cancel.session_id.clone(), InFlightSlot::Cancelled);
                        eprintln!(
                            "[darkmux-acp] session/cancel: {} — nothing in flight yet; recording a \
                             cancel tombstone in case the command hasn't registered itself yet",
                            cancel.session_id
                        );
                    }
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        // (#1684 remainder — session hygiene) `session/close`: the map-
        // pruning mechanism named in the module doc's own "never pruned"
        // finding. Per spec, close implies cancel first — reuses the SAME
        // `InFlight` registry `session/cancel` drives, above — then removes
        // the session's `sessions` entry so a later prompt on the same id
        // behaves exactly like an id this process never minted (proven at
        // the wire level by this file's own tests).
        .on_receive_request(
            async move |request: CloseSessionRequest, responder, _cx| {
                // (#1781) Closing a thread is client traffic too. The
                // ever-attached latch is what keeps this from re-arming the
                // reported bug (a close no longer makes the process look
                // never-used), and this stamp is what keeps the hard
                // ceiling honest about when the client last spoke.
                idle_for_close.record_activity();
                if let Some(InFlightSlot::Running(handle)) = in_flight_tasks_for_close
                    .lock()
                    .expect("darkmux acp: in-flight tasks mutex poisoned")
                    .remove(&request.session_id)
                {
                    handle.abort();
                }
                let existed = sessions_for_close
                    .lock()
                    .expect("darkmux acp: sessions mutex poisoned")
                    .remove(&request.session_id)
                    .is_some();
                eprintln!(
                    "[darkmux-acp] session/close: {} ({})",
                    request.session_id,
                    if existed { "pruned" } else { "already unknown" }
                );
                responder.respond(CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(transport)
        .await?;

    Ok(())
}

/// (#1684 remainder — cancellation) Run `work` as a genuinely abortable
/// `tokio::spawn`'d task, registered in `in_flight` under `session_id` for
/// the duration — the seam `session/cancel`/`session/close` abort into (see
/// [`InFlight`]'s own doc). `cx.spawn` (what `serve()`'s `PromptRequest`
/// handler already runs the whole command inside, per Packet 2's own
/// deadlock-avoidance doc) never hands back anything abortable, so
/// cancellation needs a genuinely separate `tokio::spawn`'d task underneath
/// it — this function is that task, plus the bookkeeping.
///
/// Translates the join outcome into the `StopReason` the caller's
/// `PromptResponse` should carry: `StopReason::Cancelled` when
/// `session/cancel` aborted the task before it finished (sending its own
/// "cancelled" chunk, since the caller's `work` never got the chance to
/// render anything of its own), `StopReason::EndTurn` for a normal
/// completion (whether `work` returned `Ok` or `Err` — an `Err` still ends
/// the turn, it just also renders a failure chunk first) or the rare case
/// of `work` itself panicking (also rendered as a chunk, never silently
/// swallowed — see the caller's own "nothing may panic across the protocol
/// boundary" robustness rule).
///
/// Entry removal happens unconditionally once `work` settles (success,
/// error, or abort) — `session/cancel`/`session/close` already remove the
/// entry themselves on the abort path, so this is a harmless no-op remove
/// in that case, not a double-abort risk (removing an absent key is inert).
async fn run_cancellable(
    in_flight: &InFlight,
    session_id: SessionId,
    cx: ConnectionTo<Client>,
    work: impl std::future::Future<Output = Result<()>> + Send + 'static,
) -> StopReason {
    let handle = tokio::spawn(work);
    // (#1777 merge gate — lost-cancel race) Check for a tombstone at the
    // EXACT point this would otherwise insert its own handle — see
    // `register_or_consume_cancel_tombstone`'s own doc.
    if register_or_consume_cancel_tombstone(in_flight, &session_id, handle.abort_handle()) {
        handle.abort();
    }
    let outcome = handle.await;
    in_flight
        .lock()
        .expect("darkmux acp: in-flight tasks mutex poisoned")
        .remove(&session_id);

    match outcome {
        Ok(Ok(())) => StopReason::EndTurn,
        Ok(Err(err)) => {
            eprintln!("[darkmux-acp] session/prompt: command failed: {err:#}");
            let _ = cx.send_notification(agent_chunk(&session_id, format!("darkmux acp: command failed: {err:#}")));
            StopReason::EndTurn
        }
        Err(join_err) if join_err.is_cancelled() => {
            eprintln!("[darkmux-acp] session/prompt: {session_id} cancelled via session/cancel");
            let _ = cx.send_notification(agent_chunk(&session_id, "darkmux: cancelled.".to_string()));
            StopReason::Cancelled
        }
        Err(join_err) => {
            eprintln!("[darkmux-acp] session/prompt: command task panicked: {join_err}");
            let _ = cx.send_notification(agent_chunk(
                &session_id,
                format!("darkmux acp: internal error — the command task panicked: {join_err}"),
            ));
            StopReason::EndTurn
        }
    }
}

/// (#1777 merge gate — lost-cancel race) The tombstone check/insert
/// [`run_cancellable`] performs at the exact point it would otherwise
/// register `handle` as this session's live running command — factored out
/// as a pure function of `in_flight` (no `cx`/`SessionId`-wire dependency)
/// so the race fix is unit-testable without spinning up a full ACP
/// connection. Returns `true` when a `Cancelled` tombstone was ALREADY
/// there (meaning: `session/cancel` raced ahead of this registration —
/// consume the tombstone and report "already cancelled, abort `handle`
/// immediately"), `false` when this call successfully registered `handle`
/// as the session's new [`InFlightSlot::Running`] entry (the ordinary
/// case).
fn register_or_consume_cancel_tombstone(
    in_flight: &InFlight,
    session_id: &SessionId,
    handle: tokio::task::AbortHandle,
) -> bool {
    let mut guard = in_flight.lock().expect("darkmux acp: in-flight tasks mutex poisoned");
    match guard.get(session_id) {
        Some(InFlightSlot::Cancelled) => {
            guard.remove(session_id);
            true
        }
        _ => {
            guard.insert(session_id.clone(), InFlightSlot::Running(handle));
            false
        }
    }
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
        .map(|s| s.cwd.clone())
}

/// Snapshot a session's artifact shelf + config-option overrides for one
/// ask (#1698 Packet B2, scopes C/F) — a clone under the lock, released
/// immediately, so the answering seat's (potentially slow) dispatch never
/// holds the sessions mutex.
fn session_answer_context(
    sessions: &Sessions,
    session_id: &SessionId,
) -> (crate::radio_answer::ArtifactShelf, crate::radio_answer::AnswererOverrides) {
    sessions
        .lock()
        .expect("darkmux acp: sessions mutex poisoned")
        .get(session_id)
        .map(|s| (s.shelf.clone(), s.overrides.clone()))
        .unwrap_or_default()
}

/// Push one rendered command execution onto a session's shelf (#1698 Packet
/// B2, scope C — "written on every command execution AND routed
/// execution"). A session that vanished between the execution and this call
/// (shouldn't happen — the same session id drove the execution) is a silent
/// no-op, not a panic; the shelf write is best-effort bookkeeping, not a
/// correctness-bearing step.
fn session_shelf_push(sessions: &Sessions, session_id: &SessionId, entry: crate::radio_answer::ShelfEntry) {
    if let Some(state) = sessions.lock().expect("darkmux acp: sessions mutex poisoned").get_mut(session_id) {
        state.shelf.push(entry);
    }
}

/// Turn a resolved [`crate::acp_panel::RoutePlan`] into an actual execution
/// — the SAME two-way match `serve()`'s `PromptRequest` handler ran
/// inline before #1698 Packet B, extracted so BOTH the slash-command path
/// AND the new no-slash channel (`run_no_slash_route` below) drive
/// identical behavior once a plan is resolved: same
/// `run_ephemeral_command`/`run_launch_command` execution primitives, same
/// gates, no divergence between "the operator typed `/review`" and "the
/// operator typed `review this` and the router picked `/review`".
async fn execute_route_plan(
    session_id: &SessionId,
    plan: crate::acp_panel::RoutePlan,
    args: &str,
    cwd: &Path,
    cx: &ConnectionTo<Client>,
    sessions: &Sessions,
) -> Result<()> {
    match plan {
        crate::acp_panel::RoutePlan::Ephemeral(config) => {
            run_ephemeral_command(session_id, *config, args.to_string(), cwd.to_path_buf(), cx, sessions).await
        }
        crate::acp_panel::RoutePlan::Launch(config_id) => {
            run_launch_command(session_id, &config_id, args, cwd, cx, sessions).await
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
    seat: AnsweringSeat,
    sessions: &Sessions,
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
        // (#1698 Packet B2, scope A) A router refusal no longer prints the
        // bare reason + listing directly — it goes to the ANSWERING seat
        // for a grounded, in-persona reply, with the session's real
        // artifact shelf + config-option overrides. The bare reason +
        // listing is now the LAST RESORT, rendered only when the
        // answering dispatch itself fails — see `answer_no_slash_refusal`'s
        // own doc.
        crate::radio::RouteDecision::Refuse { reason } => {
            answer_no_slash_refusal(session_id, text, &reason, cwd, cx, seat, sessions).await
        }
        // Not a refusal: the routing seat could not run at all. The answering
        // seat would fail the same way, so say it once and stop.
        crate::radio::RouteDecision::Unavailable { error } => {
            cx.send_notification(agent_chunk(session_id, format!("darkmux: could not reach a model.\n{error}")))?;
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
            execute_route_plan(session_id, plan, &args, cwd, cx, sessions).await
        }
    }
}

/// (#1698 Packet B2, scope A) Route a router refusal to the ANSWERING seat.
/// Runs on `spawn_blocking` — the answering dispatch is a synchronous,
/// potentially slow call (same shape/reason as the routing dispatch above
/// and `run_ephemeral`'s own doc). Falls back to the bare refusal reason +
/// live command listing (the pre-B2 behavior) ONLY when the answering
/// dispatch itself errors (e.g. no model loaded) — never silently drops the
/// operator's message.
async fn answer_no_slash_refusal(
    session_id: &SessionId,
    text: &str,
    refusal_reason: &str,
    cwd: &Path,
    cx: &ConnectionTo<Client>,
    seat: AnsweringSeat,
    sessions: &Sessions,
) -> Result<()> {
    let (shelf, overrides) = session_answer_context(sessions, session_id);
    // (#1698 Packet B2 gate) Resolve the data boundary BEFORE assembling —
    // the dispatch only ever sees a finished message, so this is the last
    // point at which "what may leave this machine" can still be decided.
    let scope = (seat.scope)(&overrides);
    if scope == crate::radio_answer::GroundingScope::RemoteSafe {
        eprintln!(
            "[darkmux-acp] radio answering seat resolves to a REMOTE endpoint — grounding \
             limited to the command catalog and `--help`; this machine's config surface, \
             mission board, and artifact shelf are withheld."
        );
    }
    let text_owned = text.to_string();
    let cwd_owned = cwd.to_path_buf();
    let outcome = tokio::task::spawn_blocking(move || {
        let catalog = crate::radio::compile_catalog();
        crate::radio_answer::answer(
            &text_owned,
            &catalog,
            &shelf,
            &cwd_owned,
            scope,
            crate::radio::RadioSurface::Panel,
            &mut |m: &str| (seat.call)(m, &overrides),
        )
    })
    .await
    .context("joining the radio answering task")?;

    match outcome {
        Ok(outcome) => {
            eprintln!(
                "[darkmux-acp] radio answering seat replied ({} chars; {} chars rendered)",
                outcome.text.chars().count(),
                outcome.rendered.chars().count()
            );
            Ok(cx.send_notification(agent_chunk(session_id, outcome.rendered))?)
        }
        Err(e) => {
            eprintln!("[darkmux-acp] radio answering seat failed: {e:#}; falling back to the plain refusal");
            let advertised = crate::acp_panel::list_panel_commands();
            Ok(cx.send_notification(agent_chunk(
                session_id,
                format!("{refusal_reason}\n\n{}", crate::acp_panel::not_a_command_message(&advertised)),
            ))?)
        }
    }
}

/// (#1684 rule D) Drive a procedural-only panel command's graph in-process
/// via `acp_panel::run_ephemeral` — no mission instance minted, no
/// lifecycle records. `run_ephemeral` is fully synchronous (it shells out
/// to `std::process::Command::output()` for `procedural.shell` steps), so
/// it runs on a `spawn_blocking` thread rather than the connection's own
/// async task — a "never stall the ACP event loop" concern; the retired
/// review launcher's own module doc named the equivalent tradeoff for its
/// (accepted, spike-grade) subprocess await before #2310 P4d folded it
/// into [`run_launch_command`], which awaits its `tokio::process::Command`
/// directly and needs no `spawn_blocking` of its own.
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
    sessions: &Sessions,
) -> Result<()> {
    // (#1684 Packet 2) The ACP surface's operator sign-off gate handler —
    // see `acp_gate_handler`'s own doc. Built here (on the connection's
    // async task, which owns `cx`/`session_id`) and moved into the
    // `spawn_blocking` closure below; the ephemeral runner calls it
    // synchronously from the blocking thread for any step in `config`'s
    // graph that declares `"gate": "operator"`.
    let mut gate = acp_gate_handler(cx.clone(), session_id.clone());
    let config_id = config.id.clone();
    let args_for_shelf = args.clone();
    let handle = tokio::task::spawn_blocking(move || {
        crate::acp_panel::run_ephemeral(&config, &args, &cwd, Some(&mut gate))
    });
    // (#1777 merge gate — MUST FIX 1 tier 2) Wrapped in a guard, NOT
    // awaited bare — see `EphemeralJoinGuard`'s own doc and the module
    // doc's "no OS subprocess to leak" correction. `spawn_blocking`'s
    // closure cannot itself be preempted by `session/cancel`/
    // `session/close` aborting THIS future, so a bare `.await` here would
    // silently discard whatever the closure eventually returns (a
    // `procedural.shell` step that genuinely executed — e.g. a `gh pr
    // merge` — with nothing left to report it happened). The guard
    // detaches onto an untracked task instead of losing that result.
    let outcome = EphemeralJoinGuard {
        handle: Some(handle),
        session_id: session_id.clone(),
        cx: cx.clone(),
        sessions: sessions.clone(),
        config_id: config_id.clone(),
        args: args_for_shelf.clone(),
    }
    .join()
    .await?;
    // The ACP panel surface has no exit-code concept — it just displays
    // whichever text comes back, byte-identical to before `run_ephemeral`
    // gained a typed `success` field (#1698 Packet B carry-list item 5).
    // (#1698 Packet B2, scope C) Shelved BEFORE the notification — see
    // `run_launch_command`'s own comment on the same ordering, below.
    session_shelf_push(sessions, session_id, crate::radio_answer::shelf_entry(&config_id, &args_for_shelf, &outcome.text));
    cx.send_notification(agent_chunk(session_id, outcome.text))?;
    Ok(())
}

/// (#1777 merge gate — MUST FIX 1 tier 2) Wraps the `spawn_blocking`
/// `JoinHandle` [`run_ephemeral_command`] awaits, so a `procedural.shell`
/// step that DID execute never has its result silently thrown away just
/// because `session/cancel`/`session/close` aborted the future that was
/// awaiting it — see the module doc's "no OS subprocess to leak"
/// correction for the full story on why the underlying OS process itself
/// still can't be STOPPED (`spawn_blocking`'s closure can't be preempted
/// mid-call); this only stops the eventual RESULT from vanishing.
///
/// [`Self::join`] awaits the handle in place — via `&mut JoinHandle`,
/// which is itself `Future` because `JoinHandle` is `Unpin`, so this never
/// moves the handle OUT of `self` — exactly like the bare `.await` this
/// replaces, for the normal (uncancelled) path; `self.handle` is set to
/// `None` only once that completes, disarming the drop below. If `join`'s
/// own future gets dropped before reaching that point (the abort case,
/// since `self` is captured whole by `join`'s generated state machine),
/// `Drop::drop` fires with `handle` still `Some(..)` and hands it to a
/// BRAND NEW, untracked `tokio::spawn` — never registered in [`InFlight`],
/// so no future `session/cancel`/`session/close` can reach it — which
/// posts a `"completed after cancellation: ..."` chunk once the blocking
/// work actually finishes.
struct EphemeralJoinGuard {
    handle: Option<tokio::task::JoinHandle<Result<crate::acp_panel::EphemeralOutcome>>>,
    session_id: SessionId,
    cx: ConnectionTo<Client>,
    sessions: Sessions,
    config_id: String,
    args: String,
}

impl EphemeralJoinGuard {
    /// Await the blocking task to completion. Called at most once — see
    /// the struct's own doc on why `self` must stay intact (handle
    /// `Some`) for the ENTIRE suspension, only clearing it once this
    /// actually resolves.
    async fn join(mut self) -> Result<crate::acp_panel::EphemeralOutcome> {
        let joined = self
            .handle
            .as_mut()
            .expect("EphemeralJoinGuard::join is only ever called once")
            .await;
        self.handle = None; // disarm: reached completion normally, not via abort.
        joined.context("joining the ephemeral panel-command task")?
    }
}

impl Drop for EphemeralJoinGuard {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return; // `join` completed normally — nothing to detach.
        };
        eprintln!(
            "[darkmux-acp] ephemeral command `{}` was cancelled while its subprocess step kept \
             running (a `spawn_blocking` closure cannot be preempted) — detaching to report the \
             eventual result once it lands",
            self.config_id
        );
        let session_id = self.session_id.clone();
        let cx = self.cx.clone();
        let sessions = self.sessions.clone();
        let config_id = self.config_id.clone();
        let args = self.args.clone();
        tokio::spawn(async move {
            match handle.await {
                Ok(Ok(outcome)) => {
                    let text = format!("completed after cancellation: {}", outcome.text);
                    session_shelf_push(
                        &sessions,
                        &session_id,
                        crate::radio_answer::shelf_entry(&config_id, &args, &text),
                    );
                    let _ = cx.send_notification(agent_chunk(&session_id, text));
                }
                Ok(Err(err)) => {
                    eprintln!(
                        "[darkmux-acp] ephemeral command `{config_id}` finished after cancellation \
                         with an error: {err:#}"
                    );
                    let _ = cx.send_notification(agent_chunk(
                        &session_id,
                        format!("completed after cancellation: `{config_id}` failed: {err:#}"),
                    ));
                }
                Err(join_err) => {
                    eprintln!(
                        "[darkmux-acp] ephemeral command `{config_id}`'s blocking task panicked \
                         after cancellation: {join_err}"
                    );
                }
            }
        });
    }
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
    // `SessionUpdate::ToolCall` before this request — unlike the retired
    // review launcher's own stage tool calls (`stage_tool_call_id`,
    // deleted with that launcher in #2310 P4d), which always sent a
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

/// Load a mission config by the id `route_command` resolved, for the
/// launch-time input synthesis above. Separate from `route_command`'s own
/// load because the two run at different moments (routing, then spawning)
/// and the registry is the source of truth at each.
fn loaded_config(config_id: &str) -> Result<crate::crew::mission_config::MissionConfig> {
    crate::crew::mission_config::load(config_id)
        .map(|l| l.config)
        .with_context(|| format!("loading mission config \"{config_id}\""))
}

/// (#2476 review round 2, MUST FIX 2) The pids of every currently-live
/// [`spawn_registered`] child — a SUBSET of `darkmux_types::
/// child_registry`'s own process-wide set, used only to single those
/// pids out for the gentler shutdown treatment [`run`]'s production
/// closure gives them (SIGTERM + a bounded wait, never the blanket
/// SIGKILL `child_registry::kill_all` sends everything else) — see
/// [`SpawnedLaunchChild`]'s and that closure's own docs for why.
static LAUNCH_CHILDREN: Mutex<std::collections::BTreeSet<u32>> = Mutex::new(std::collections::BTreeSet::new());

/// (#2476 review round 2, MUST FIX 1 + MUST FIX 2) RAII guard for one
/// [`spawn_registered`] child pid — registers it in BOTH the
/// process-wide `child_registry` (so [`host_shutdown_reap_loop`]'s reap
/// sweep can reach it at all) and this file's own [`LAUNCH_CHILDREN`]
/// (so that sweep's production closure can single it out for a graceful
/// SIGTERM-then-wait instead of the blanket SIGKILL every OTHER
/// registered dispatch child gets — see that closure's own doc). `Drop`
/// deregisters BOTH unconditionally, mirroring `dispatch_internal.rs`'s
/// `PidRegistration` — the SAME convention this function's doc already
/// claimed to follow, except the ORIGINAL `spawn_registered` only
/// deregistered via a plain statement placed AFTER the awaited
/// `wait_with_output()`, which `session/cancel`'s and `session/close`'s
/// `handle.abort()` never reach: `Handle::abort()` drops the awaited
/// future AT its suspended await point, skipping every statement after
/// it, so a CANCELLED launch left its pid registered forever in a
/// process-wide set while the underlying OS process (reaped by
/// `kill_on_drop`) was already gone and its pid free for the kernel to
/// recycle onto an unrelated process — exactly the hazard `child_registry
/// ::kill_pid`'s own doc warns a caller about. RAII closes that: `Drop`
/// runs on every exit path, abort included, the same guarantee
/// `PidRegistration` gives `dispatch_internal.rs`'s docker/curl children.
struct SpawnedLaunchChild(u32);

impl SpawnedLaunchChild {
    fn new(pid: u32) -> Self {
        darkmux_types::child_registry::register(pid);
        if let Ok(mut set) = LAUNCH_CHILDREN.lock() {
            set.insert(pid);
        }
        Self(pid)
    }
}

impl Drop for SpawnedLaunchChild {
    fn drop(&mut self) {
        darkmux_types::child_registry::deregister(self.0);
        if let Ok(mut set) = LAUNCH_CHILDREN.lock() {
            set.remove(&self.0);
        }
    }
}

/// (#2476) Spawn `cmd`, registering its pid (via [`SpawnedLaunchChild`])
/// for the span between spawn and the wait resolving — the SAME
/// "register before the blocking wait" convention `dispatch_internal.rs`'s
/// `PidRegistration` established for the synchronous docker/curl children
/// `crew::dispatch` spawns (see that struct's own doc, and
/// `SpawnedLaunchChild`'s doc for why THIS function now uses an RAII
/// guard too rather than a plain register/deregister pair).
/// [`run_launch_command`]'s `mission launch` subprocess already had
/// `.kill_on_drop(true)` for ACP-level `session/cancel` aborts (dropping
/// the awaiting task drops the `Child`), but an OS SIGINT/SIGTERM to
/// `darkmux acp` ITSELF never reached that `Drop` at all before this fix —
/// an unhandled signal tore the whole process down before any Rust
/// destructor could run, orphaning this subprocess. Registering here is
/// what lets this file's own host shutdown handling
/// ([`host_shutdown_reap_loop`], below) reach it — the router and answerer
/// curl dispatches need no equivalent change: they already register
/// through `darkmux-crew`'s own `remote_chat_attempt`/
/// `dispatch_local_single_shot`, shared, unmodified code this file
/// already calls.
async fn spawn_registered(mut cmd: Command) -> std::io::Result<std::process::Output> {
    let child = cmd.spawn()?;
    let pid = child.id();
    let _guard = pid.map(SpawnedLaunchChild::new);
    child.wait_with_output().await
}

/// (#2476) Wait for SIGINT or SIGTERM — mirrors `darkmux-serve`'s own
/// `shutdown_signal()`. Matches that daemon's existing two-signal scope
/// rather than the three-signal (+ SIGHUP) set `mission launch` launchers
/// install via `launch_guard::arm()`; widening to SIGHUP here is a
/// reasonable future addition, not something this fix's own scope (a
/// missing guard, not missing signal breadth) requires.
///
/// **Why not `launch_guard::arm()`/`darkmux_types::interrupt::
/// install_term()`.** Those call raw `libc::signal(2)`, which
/// unconditionally REPLACES whatever handler currently owns a signal's
/// disposition — installing one here would silently break `tokio::
/// signal`'s own registration for any OTHER listener started later in
/// this same process (`tokio::signal` uses `signal-hook-registry`, which
/// chains cooperating listeners; a raw `libc::signal()` call does not
/// cooperate, it overwrites). Reusing `tokio::signal` end-to-end, the
/// same primitive `darkmux serve` already uses successfully, avoids that
/// interaction entirely. See `darkmux_types::interrupt::mark_interrupted`'s
/// own doc for the fuller version of this reasoning.
///
/// **The `ready` parameter (#2476 review round 2, CONSIDER 7).** `None`
/// in every production call ([`host_shutdown_reap_loop_ready`], which
/// [`host_shutdown_reap_loop`] delegates to with `None`) — when `Some`,
/// it fires the instant the unix `SIGTERM` listener is installed, BEFORE
/// this function ever awaits a signal on it. Two tests below self-signal
/// (`kill -TERM` their OWN process) right after spawning a task that
/// waits on this function; without a latch they would bet a fixed sleep
/// is long enough for `tokio::signal`'s listener to have actually
/// registered first. On a loaded box that bet can lose: an unregistered
/// `SIGTERM` takes default disposition, which does not just fail the one
/// test — it kills the WHOLE test BINARY outright. The latch removes the
/// bet, and changes nothing about the control flow production sees.
async fn wait_for_host_shutdown_signal_ready(ready: Option<tokio::sync::oneshot::Sender<()>>) {
    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{signal, SignalKind};
        let sig = signal(SignalKind::terminate());
        if let Some(tx) = ready {
            let _ = tx.send(()); // best-effort — a dropped receiver means the test stopped watching, not a bug here.
        }
        if let Ok(mut sig) = sig {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = async {
        if let Some(tx) = ready {
            let _ = tx.send(());
        }
        std::future::pending::<()>().await
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term => {},
    }
}

/// (#2476) Wait for a host shutdown signal, then invoke `on_signal` —
/// split out so a test can inject a callback that does NOT call
/// `std::process::exit` and still prove the wiring against a REAL
/// delivered signal without killing the test binary. Production
/// ([`run`]) passes [`reap_on_host_shutdown`], which SIGKILLs every
/// registered dispatch child EXCEPT any `mission launch` child (this
/// file's own via [`spawn_registered`]; `darkmux-crew`'s own curl/docker
/// children, registered independently of this file, get no such
/// exception) — see `reap_on_host_shutdown`'s own doc (#2476 review
/// round 2, MUST FIX 2) for why the launch child is deliberately
/// carved out rather than reached by the same blanket sweep. See this
/// module's own doc for why `darkmux acp` needed this at all: before
/// this fix, a SIGTERM mid-dispatch killed the process outright (default
/// disposition, no handler installed anywhere in this file) and orphaned
/// whatever child was in flight.
///
/// **Why this host EXITS on a caught signal rather than continuing to
/// serve** (the design question #2476 itself raised — "worth deciding
/// whether `interrupt`'s never-reset behavior should stay global"):
/// `darkmux_types::interrupt::is_set()` never resets, so
/// `launch_guard::spawn_reap_watchdog`'s forever-loop would be actively
/// hostile if this process kept accepting new work after tripping once —
/// every LATER dispatch's children would get SIGKILLed too. That risk
/// only exists for a host that survives the signal. This one doesn't: an
/// operator-delivered SIGINT/SIGTERM to a long-lived host process is
/// conventionally "stop", exactly like `darkmux serve`'s own shutdown
/// path already treats it — so `is_set()`'s never-reset contract costs
/// nothing here, and no scoped/resettable variant needed to be built.
async fn host_shutdown_reap_loop(on_signal: impl FnOnce() + Send + 'static) {
    host_shutdown_reap_loop_ready(on_signal, None).await
}

/// (#2476 review round 2, CONSIDER 7) Same as [`host_shutdown_reap_loop`],
/// threading an optional readiness latch through to
/// [`wait_for_host_shutdown_signal_ready`] — see that function's own doc.
/// Production ([`host_shutdown_reap_loop`], above) always passes `None`.
async fn host_shutdown_reap_loop_ready(
    on_signal: impl FnOnce() + Send + 'static,
    ready: Option<tokio::sync::oneshot::Sender<()>>,
) {
    wait_for_host_shutdown_signal_ready(ready).await;
    on_signal();
}

/// (#1684 rule D) Launch a panel command whose graph has at least one
/// model-dispatching step as a normal `darkmux mission launch <id>`
/// subprocess — a full instance (this process's own executable,
/// re-invoked, cwd = the session's cwd, stdout/stderr captured as pipes —
/// never inherited, so nothing but this file's own `agent_chunk`
/// notifications reaches the ACP wire). There is no bespoke stage/
/// liveness parsing here — the retired review launcher had its own
/// (`REVIEW_STAGES`/`recognize_stage`, deleted in #2310 P4d); this generic
/// path renders its subprocess's stdout (trimmed) as the final message on
/// success, or its stderr on failure. Sends an up-front "launched…" chunk
/// before awaiting the subprocess (#1684 QA finding — CONSIDER 12), so
/// Zed shows something other than a bare spinner for however long the
/// launched mission's own model dispatches take (the retired review
/// launcher streamed a `Plan` immediately for the same reason).
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
    sessions: &Sessions,
) -> Result<()> {
    let exe = std::env::current_exe().context("resolving darkmux's own executable path")?;
    let mut cmd = Command::new(&exe);
    cmd.args(["mission", "launch", config_id]);
    if !args.trim().is_empty() {
        cmd.args(["--param", &format!("args={args}")]);
    }

    // (#2310 P4d) A diff-scoped config (`review`) declares `diff_file`
    // REQUIRED, and a panel invocation types no params — synthesize the
    // diff + workspace from this session's cwd, exactly as the retired
    // review launcher used to before it spawned. `_synth` is held for the
    // whole spawn: its Drop removes the tempdir, so every exit path below
    // (success, failure, a cancelled subprocess) cleans up.
    let _synth = match crate::acp_panel::synthesize_diff_launch_inputs(&loaded_config(config_id)?, cwd) {
        Ok(crate::acp_panel::DiffLaunchInputs::NotNeeded) => None,
        Ok(crate::acp_panel::DiffLaunchInputs::Nothing(msg)) => {
            cx.send_notification(agent_chunk(session_id, msg))?;
            return Ok(());
        }
        Ok(crate::acp_panel::DiffLaunchInputs::Ready(synth)) => {
            for p in synth.params() {
                cmd.args(["--param", p]);
            }
            if let Some(note) = &synth.excluded_note {
                let _ = cx.send_notification(agent_chunk(session_id, format!("darkmux: {note}")));
            }
            Some(synth)
        }
        Err(e) => {
            cx.send_notification(agent_chunk(session_id, format!("darkmux: `{config_id}` cannot run here — {e:#}")))?;
            return Ok(());
        }
    };

    eprintln!(
        "[darkmux-acp] session/prompt: spawning `mission launch {config_id}` cwd={}",
        cwd.display()
    );
    let _ = cx.send_notification(agent_chunk(session_id, format!("darkmux: launching `{config_id}`…")));

    cmd.current_dir(cwd)
        .stdin(ProcStdio::null())
        .stdout(ProcStdio::piped())
        .stderr(ProcStdio::piped())
        // (#1684 remainder — cancellation) A `session/cancel`-driven abort
        // drops this `Child` mid-`.output()`; `kill_on_drop(true)` sends the
        // OS process a real kill rather than orphaning it.
        .kill_on_drop(true);
    // (#2476) `spawn_registered` (not a plain `.output()`) so this pid is
    // also reachable from `child_registry::kill_all` — see its own doc
    // for why `kill_on_drop` alone isn't enough for an OS-signal-driven
    // shutdown of this process.
    let output = spawn_registered(cmd)
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
    // (#1698 Packet B2, scope C) Shelved BEFORE the notification — so a
    // shelf read that races the notification still sees this exchange.
    session_shelf_push(sessions, session_id, crate::radio_answer::shelf_entry(config_id, args, &text));
    cx.send_notification(agent_chunk(session_id, text))?;
    Ok(())
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    /// (#1781) Build an `IdleState` whose activity mark is a KNOWN zero, so
    /// the `now_secs` a test hands `should_idle_exit` IS the idle duration —
    /// never a function of how long the test itself took to reach the
    /// assertion. (`IdleState::new` already starts at zero; the explicit
    /// store is what keeps that true after a `record_*` call.)
    fn idle_state_idle_since_zero() -> IdleState {
        let state = IdleState::new();
        state.last_activity.store(0, Ordering::SeqCst);
        state
    }

    /// (#1781) `should_idle_exit` is the whole decision `idle_self_exit_loop`
    /// acts on, taking the REAL `IdleState` the loop hands it — so these
    /// prove it without touching `std::process::exit`, and so the loop
    /// cannot be reverted past them by passing a stand-in (see
    /// `IdleState`'s own doc on the literal that reverted the previous
    /// shape).
    ///
    /// The case the issue is actually about: a session left open but idle
    /// past the configured window must NOT trigger the exit — that is what
    /// bricked the Zed panel, because the loop used to look only at elapsed
    /// time and the in-flight count.
    #[test]
    fn should_idle_exit_never_fires_once_a_session_has_attached() {
        let state = idle_state_idle_since_zero();
        state.record_session_attached();
        state.last_activity.store(0, Ordering::SeqCst);
        // Idle time and in-flight are BOTH in the "would exit" shape for
        // the CONFIGURED window — only the attached latch holds it back.
        assert!(
            !should_idle_exit(&state, 3600, 1800),
            "an attached (merely idle) session must never be killed at the configured window"
        );
    }

    /// (#1781, the predicate's whole point) `session/close` prunes the
    /// session from the map, so "no session open right now" goes true again
    /// while the client, its transport and its workspace are all still
    /// live — close a thread, wait out the window, open a new one and the
    /// reported `Incoming transport closed` comes back verbatim on a second
    /// path. The latch is set by attaching and is never cleared by
    /// anything, close included (which only records activity and prunes the
    /// map), so that path is closed by construction.
    #[test]
    fn a_closed_session_keeps_the_latch_so_the_reported_bug_cannot_recur() {
        let state = idle_state_idle_since_zero();
        state.record_session_attached();
        // Everything `session/close` does to this state: record the
        // traffic. (Pruning happens in the `sessions` map, which this
        // decision deliberately no longer reads.)
        state.record_activity();
        state.last_activity.store(0, Ordering::SeqCst);
        assert!(
            !should_idle_exit(&state, 3600, 1800),
            "a process whose only session was closed must still not be reclaimed at the \
             configured window — that is the #1781 symptom on a second path"
        );
    }

    /// The command-in-flight guard, which `should_idle_exit` now evaluates
    /// itself rather than relying on the loop to short-circuit ahead of it —
    /// so this pins the LIVE guard, not a branch the loop never reaches.
    /// Set up in the strongest "would exit" shape available: never
    /// attached, idle window long past.
    #[test]
    fn should_idle_exit_never_fires_with_a_command_in_flight() {
        let state = idle_state_idle_since_zero();
        state.command_started();
        assert!(!should_idle_exit(&state, 3600, 1800));
    }

    /// Idle time under the configured threshold must not fire even with
    /// everything else in the "would exit" shape — the loop is time-gated,
    /// not just presence-gated.
    #[test]
    fn should_idle_exit_never_fires_before_the_threshold() {
        let state = idle_state_idle_since_zero();
        assert!(!should_idle_exit(&state, 1799, 1800));
    }

    /// The one case the backstop actually exists for (module doc's #1781
    /// note): nothing ever attached a session to this process, nothing is
    /// running, and it has sat idle past the configured window. This is the
    /// reclaim case — removing it entirely (rather than gating it) was
    /// rejected because a spawned-and-abandoned process would otherwise
    /// never exit.
    #[test]
    fn should_idle_exit_reclaims_a_process_no_session_ever_attached() {
        let state = idle_state_idle_since_zero();
        assert!(should_idle_exit(&state, 1800, 1800));
        assert!(should_idle_exit(&state, 3600, 1800));
    }

    /// (#1781) The leak is BOUNDED, not merely accepted: a process that was
    /// attached once and has heard nothing since is still reclaimed — just
    /// at the week-scale hard ceiling rather than the configured window, far
    /// past any plausible "I'll come back to that panel".
    #[test]
    fn should_idle_exit_reclaims_an_attached_process_at_the_hard_ceiling() {
        let state = idle_state_idle_since_zero();
        state.record_session_attached();
        state.last_activity.store(0, Ordering::SeqCst);
        let hard = hard_idle_threshold(1800);
        assert!(
            !should_idle_exit(&state, hard - 1, 1800),
            "one second under the ceiling must still not fire"
        );
        assert!(
            should_idle_exit(&state, hard, 1800),
            "at the ceiling the orphan is finally reclaimed"
        );
    }

    /// (#1781) The ceiling's floor is what keeps an operator who shortens
    /// the reclaim window — the knob's documented purpose is
    /// spawned-but-unused processes — from also arming a short kill on the
    /// panel they are reading, and what keeps the shipped default well
    /// clear of ordinary use. See `hard_idle_threshold`'s own doc for why
    /// the floor is a week rather than a day.
    #[test]
    fn hard_idle_threshold_never_drops_below_a_week() {
        const WEEK: u64 = 7 * 24 * 60 * 60;
        assert_eq!(hard_idle_threshold(30 * 60), WEEK, "the shipped 30-minute default lands on the floor");
        assert_eq!(hard_idle_threshold(60), WEEK, "a one-minute window still gets a full week");
        assert_eq!(hard_idle_threshold(0), WEEK, "and the floor holds even at zero");
        assert_eq!(
            hard_idle_threshold(8 * 60 * 60),
            8 * 60 * 60 * HARD_IDLE_MULTIPLIER,
            "above the floor the multiplier is what applies"
        );
        assert_eq!(hard_idle_threshold(u64::MAX), u64::MAX, "and the multiply saturates rather than wrapping");
    }

    /// (#1861 review) This file's PRODUCTION half — everything ahead of
    /// its own test module. Searched instead of the whole file on purpose:
    /// a needle spelled out in a test must never be able to satisfy an
    /// assertion about the production call site.
    fn production_source() -> &'static str {
        let src = include_str!("acp.rs");
        let cut = src.find("#[cfg(test)]").expect("this file has a test module");
        &src[..cut]
    }

    /// (#1861 review) ACP's two answering call sites — `run()`'s
    /// `AnswererCall` closure and `answer_no_slash_refusal`'s `answer()`
    /// call — both CLAIM the panel surface in their comments. Neither was
    /// pinned: flipping either literal to `Cli` would make the panel start
    /// rewriting its own real `/id` references as invented, with every
    /// test green. Both sites need a live connection to exercise, so the
    /// claim is pinned against the artifact that makes it.
    #[test]
    fn both_acp_answering_call_sites_pin_the_panel_surface() {
        let src = production_source();
        assert!(
            src.contains("dispatch_answerer_call_with(m, overrides, crate::radio::RadioSurface::Panel)"),
            "run()'s answerer closure must pin the panel surface"
        );
        assert!(
            src.contains("            scope,\n            crate::radio::RadioSurface::Panel,"),
            "answer_no_slash_refusal must pass the panel surface to `answer()`"
        );
        assert!(
            !src.contains("RadioSurface::Cli"),
            "ACP is the panel surface by construction — nothing here may claim the CLI's"
        );
    }

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

    /// Write one panel-advertised, procedural-only fixture command whose
    /// SINGLE step is a real `procedural.shell` — an actual OS subprocess,
    /// unlike `write_echo_fixture`'s in-process `procedural.noop` — so a
    /// test can prove things about REAL child-process lifecycle (started,
    /// still running, killed) rather than in-process control flow. The
    /// command touches `marker_path` the instant it starts (so a test can
    /// poll for that file to know the subprocess is genuinely running
    /// before acting on it), sleeps `sleep_secs`, then echoes `output`.
    fn write_slow_shell_fixture(crew_dir: &Path, id: &str, marker_path: &Path, sleep_secs: u64, output: &str) {
        let dir = crew_dir.join("mission-configs");
        std::fs::create_dir_all(&dir).unwrap();
        let command = format!(
            "touch '{}' && sleep {sleep_secs} && echo '{output}'",
            marker_path.display()
        );
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_string(&serde_json::json!({
                "id": id,
                "name": id,
                "panel": {"description": "Pipe-level test fixture — a real, observable OS subprocess."},
                "phases": [{
                    "id": "p1",
                    "tasks": [{"id": "t1", "steps": [{"id": "s1", "kind": "procedural.shell", "config": {"command": command}}]}]
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
    /// `answerer` is a SEPARATE canned closure from `router` (#1698 Packet
    /// B2) — a router refusal now routes to the answering seat's OWN
    /// dispatch, a second independent model call, so pipe-level tests that
    /// never expect a refusal (or that assert on the router's own call
    /// count) inject an answerer that panics if reached, while the two
    /// refusal-path tests inject a real canned reply.
    fn spawn_test_agent(
        router: impl Fn(&str) -> Result<String> + Send + Sync + 'static,
        answerer: impl Fn(&str, &crate::radio_answer::AnswererOverrides) -> Result<String> + Send + Sync + 'static,
    ) -> (DuplexStream, BufReader<DuplexStream>) {
        let (writer, reader, _idle) = spawn_test_agent_observing_idle(router, answerer);
        (writer, reader)
    }

    /// (#1781) Same spawn, additionally handing back the `IdleState` the
    /// served connection is wired to — the seam that lets a wire test
    /// assert the ever-attached latch is actually SET by `session/new` /
    /// `session/load`, rather than only that `should_idle_exit` would
    /// respect it if it were.
    fn spawn_test_agent_observing_idle(
        router: impl Fn(&str) -> Result<String> + Send + Sync + 'static,
        answerer: impl Fn(&str, &crate::radio_answer::AnswererOverrides) -> Result<String> + Send + Sync + 'static,
    ) -> (DuplexStream, BufReader<DuplexStream>, Arc<IdleState>) {
        let (test_writer, agent_reader) = tokio::io::duplex(64 * 1024);
        let (agent_writer, test_reader) = tokio::io::duplex(64 * 1024);
        let router_call: RouterCall = Arc::new(router);
        let answerer_call: AnswererCall = Arc::new(answerer);
        // Pinned to `Full` (#1698 Packet B2 gate): these tests exercise the
        // WIRE, not the data boundary — see `ScopeCall`'s own doc.
        let scope_call: ScopeCall = Arc::new(|_| crate::radio_answer::GroundingScope::Full);
        let transport = ByteStreams::new(agent_writer.compat_write(), agent_reader.compat());
        let idle = Arc::new(IdleState::new());
        let idle_for_serve = idle.clone();
        tokio::spawn(async move {
            let _ = serve(
                router_call,
                AnsweringSeat { call: answerer_call, scope: scope_call },
                idle_for_serve,
                transport,
            )
            .await;
        });
        (test_writer, BufReader::new(test_reader), idle)
    }

    /// The default answerer for tests that never expect the answering seat
    /// to be reached — panics loudly rather than silently dispatching a
    /// live model, same "fail loud, not quiet" contract `router`'s own
    /// panic-on-call fixtures already use in this module.
    fn never_answer(_msg: &str, _overrides: &crate::radio_answer::AnswererOverrides) -> Result<String> {
        panic!("the answering seat must not be reached by this scenario");
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
        let (mut writer, mut reader) = spawn_test_agent(router, never_answer);
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

    #[tokio::test]
    #[serial_test::serial]
    async fn no_slash_unavailable_prints_once_and_never_reaches_the_answering_seat() {
        // First-run probes (2026-08-28): with no registry / a placeholder
        // model / no `lms` / the server down, the routing seat fails, the
        // failure was recast as a "refusal", the answering seat ran and
        // failed identically, and the user read the same error twice. The
        // answerer here PANICS if called: that is the assertion.
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        let flows_tmp = tempfile::TempDir::new().unwrap();
        let _flows_guard = EnvGuard::set("DARKMUX_FLOWS_DIR", flows_tmp.path());
        write_echo_fixture(crew_tmp.path(), "echo-fixture", "fixture output");

        let router = |_msg: &str| -> Result<String> {
            Err(anyhow::anyhow!("darkmux: profile `balanced` still names the placeholder `<your-worker-model-id>`"))
        };
        let answerer = |_msg: &str, _overrides: &crate::radio_answer::AnswererOverrides| -> Result<String> {
            panic!("the answering seat must not run when the routing seat could not reach a model")
        };
        let (mut writer, mut reader) = spawn_test_agent(router, answerer);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        send_prompt(&mut writer, &session_id, "what can you do?").await;

        let reply = recv_json(&mut reader).await;
        let text = chunk_text(&reply);
        assert!(text.contains("could not reach a model"), "{text}");
        assert!(text.contains("<your-worker-model-id>"), "the producer's own fix line must reach the user: {text}");
        assert_eq!(text.matches("<your-worker-model-id>").count(), 1, "printed once, not per seat: {text}");

        let final_response = recv_json(&mut reader).await;
        assert_end_turn(&final_response);

        let day = darkmux_flow::day_utc_now();
        let flow_path = flows_tmp.path().join(format!("{day}.jsonl"));
        let flow_contents = std::fs::read_to_string(&flow_path).expect("wall 4's flow record file must exist");
        assert!(flow_contents.contains("\"decision\":\"unavailable\""), "{flow_contents}");
    }

    /// (#1698 Packet B2) A router refusal routes to the ANSWERING seat — a
    /// SEPARATE canned dispatch, never the raw refusal reason rendered
    /// directly (that's the pre-B2 behavior, now the last-resort fallback
    /// only). Still records wall 4's flow record for the ROUTING decision
    /// (as a refusal, not a route) — wall 4 is about the router's own
    /// outcome, unaffected by what happens downstream at the answering seat.
    #[tokio::test]
    #[serial_test::serial]
    async fn no_slash_refusal_routes_to_the_answering_seat_and_records_wall_4() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        let flows_tmp = tempfile::TempDir::new().unwrap();
        let _flows_guard = EnvGuard::set("DARKMUX_FLOWS_DIR", flows_tmp.path());
        write_echo_fixture(crew_tmp.path(), "echo-fixture", "fixture output");

        let router = |_msg: &str| -> Result<String> {
            Ok("```json\n{\"refuse\": \"that's outside the scope of mission comms\"}\n```".to_string())
        };
        let answerer = |_msg: &str, _overrides: &crate::radio_answer::AnswererOverrides| -> Result<String> {
            Ok("RADIO: that's outside my mission comms scope too.".to_string())
        };
        let (mut writer, mut reader) = spawn_test_agent(router, answerer);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        send_prompt(&mut writer, &session_id, "what's the weather like on mars?").await;

        let reply = recv_json(&mut reader).await;
        let text = chunk_text(&reply);
        assert!(
            text.contains("that's outside my mission comms scope too"),
            "the ANSWERING seat's canned reply must be what renders, not the raw router refusal reason: {text}"
        );

        let final_response = recv_json(&mut reader).await;
        assert_end_turn(&final_response);

        let day = darkmux_flow::day_utc_now();
        let flow_path = flows_tmp.path().join(format!("{day}.jsonl"));
        let flow_contents = std::fs::read_to_string(&flow_path).expect("wall 4's flow record file must exist");
        assert!(flow_contents.contains("\"action\":\"radio.route\""), "{flow_contents}");
        assert!(flow_contents.contains("\"decision\":\"refuse\""), "{flow_contents}");
        assert!(
            flow_contents.contains("that's outside the scope of mission comms"),
            "wall 4 still records the ROUTER's own refusal reason, independent of the \
             answering seat's downstream reply: {flow_contents}"
        );
    }

    /// A last-resort fallback specimen: when the ANSWERING seat's own
    /// dispatch fails (e.g. no model loaded), the bare refusal reason +
    /// live command listing render — the pre-B2 behavior, now scoped to
    /// exactly this failure path.
    #[tokio::test]
    #[serial_test::serial]
    async fn no_slash_refusal_falls_back_to_the_plain_listing_when_the_answering_seat_errors() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        write_echo_fixture(crew_tmp.path(), "echo-fixture", "fixture output");

        let router = |_msg: &str| -> Result<String> {
            Ok("```json\n{\"refuse\": \"that's outside the scope of mission comms\"}\n```".to_string())
        };
        let answerer = |_msg: &str, _overrides: &crate::radio_answer::AnswererOverrides| -> Result<String> {
            Err(anyhow::anyhow!("no model loaded"))
        };
        let (mut writer, mut reader) = spawn_test_agent(router, answerer);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        send_prompt(&mut writer, &session_id, "what's the weather like on mars?").await;

        let fallback = recv_json(&mut reader).await;
        let text = chunk_text(&fallback);
        assert!(text.contains("that's outside the scope of mission comms"), "{text}");
        assert!(text.contains("echo-fixture"), "the live command listing follows the reason: {text}");

        let final_response = recv_json(&mut reader).await;
        assert_end_turn(&final_response);
    }

    /// (#1698 Packet B2, scope C — the shelf round trip) A command's
    /// rendered output, once executed, is visible to a LATER answering-seat
    /// dispatch in the same session — the shelf's entire reason for
    /// existing. Routes `/echo-fixture` first (pushing its output onto the
    /// shelf), then sends a no-slash message the canned router refuses; the
    /// canned ANSWERER captures the assembled message it received and this
    /// test asserts the fixture's earlier output is inside it.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_prior_commands_output_reaches_the_answering_seats_grounding_via_the_shelf() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        write_echo_fixture(crew_tmp.path(), "echo-fixture", "the-shelf-marker-output");

        let router = |_msg: &str| -> Result<String> {
            Ok("```json\n{\"refuse\": \"ambiguous\"}\n```".to_string())
        };
        let received_message: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let received_message_for_answerer = received_message.clone();
        let answerer = move |msg: &str, _overrides: &crate::radio_answer::AnswererOverrides| -> Result<String> {
            *received_message_for_answerer.lock().unwrap() = Some(msg.to_string());
            Ok("RADIO: acknowledged.".to_string())
        };
        let (mut writer, mut reader) = spawn_test_agent(router, answerer);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        // First: a SLASH invocation, executed directly (never touches the
        // router or the answerer) — pushes its output onto the shelf.
        send_prompt(&mut writer, &session_id, "/echo-fixture").await;
        let slash_output = recv_json(&mut reader).await;
        assert_eq!(chunk_text(&slash_output), "the-shelf-marker-output");
        let slash_final = recv_json(&mut reader).await;
        assert_end_turn(&slash_final);

        // Second: a no-slash message the canned router refuses, landing at
        // the answering seat with the shelf now non-empty.
        send_prompt(&mut writer, &session_id, "what did that just do?").await;
        let answer_chunk = recv_json(&mut reader).await;
        assert_eq!(chunk_text(&answer_chunk), "RADIO: acknowledged.");
        let answer_final = recv_json(&mut reader).await;
        assert_end_turn(&answer_final);

        let captured = received_message.lock().unwrap().clone().expect("answerer must have been called");
        assert!(
            captured.contains("the-shelf-marker-output"),
            "the shelf entry from the earlier /echo-fixture run must reach the answering \
             seat's assembled message: {captured}"
        );
    }

    /// (#1698 Packet B2, scope F — the overrides round trip) A
    /// `session/set_config_option` change (the "humor" picker) actually
    /// reaches the answering seat's dispatch on a LATER prompt in the same
    /// session — proving the session-scoped override isn't just stored and
    /// echoed back, but genuinely consulted at answer time.
    #[tokio::test]
    #[serial_test::serial]
    async fn set_config_option_override_reaches_the_answering_seats_dispatch() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        write_echo_fixture(crew_tmp.path(), "echo-fixture", "fixture output");

        let router = |_msg: &str| -> Result<String> {
            Ok("```json\n{\"refuse\": \"ambiguous\"}\n```".to_string())
        };
        let received_overrides: Arc<Mutex<Option<crate::radio_answer::AnswererOverrides>>> = Arc::new(Mutex::new(None));
        let received_overrides_for_answerer = received_overrides.clone();
        let answerer = move |_msg: &str, overrides: &crate::radio_answer::AnswererOverrides| -> Result<String> {
            *received_overrides_for_answerer.lock().unwrap() = Some(overrides.clone());
            Ok("RADIO: acknowledged.".to_string())
        };
        let (mut writer, mut reader) = spawn_test_agent(router, answerer);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        send_json(
            &mut writer,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
                "params": {"sessionId": session_id, "configId": "humor", "value": "90"}
            }),
        )
        .await;
        let set_response = recv_json(&mut reader).await;
        assert!(set_response.get("result").is_some(), "{set_response}");

        send_prompt(&mut writer, &session_id, "what's the weather like on mars?").await;
        let _answer_chunk = recv_json(&mut reader).await;
        let final_response = recv_json(&mut reader).await;
        assert_end_turn(&final_response);

        let captured = received_overrides.lock().unwrap().clone().expect("answerer must have been called");
        assert_eq!(
            captured,
            crate::radio_answer::AnswererOverrides { profile_name: None, humor: Some(90) },
            "the session's humor override (set via session/set_config_option) must reach the \
             answering seat's dispatch"
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
        let (mut writer, mut reader) = spawn_test_agent(router, never_answer);
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
        // (#1698 Packet B2) A refusal now routes to the answering seat, a
        // SEPARATE canned dispatch — the raw router refusal reason
        // ("ambiguous...") is never rendered directly, so this scenario
        // needs its own canned reply rather than asserting on the router's
        // own text.
        let answerer = |_msg: &str, _overrides: &crate::radio_answer::AnswererOverrides| -> Result<String> {
            Ok("RADIO: I can't tell what you meant by that.".to_string())
        };
        let (mut writer, mut reader) = spawn_test_agent(router, answerer);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        // Bare — no leading slash — and spells the fixture's OWN advertised
        // command id exactly. Pre-#1698-Packet-B `parse_command` would have
        // matched this as a bare-word command invocation.
        send_prompt(&mut writer, &session_id, "echo-fixture").await;

        let refusal = recv_json(&mut reader).await;
        assert!(
            chunk_text(&refusal).contains("I can't tell what you meant"),
            "a bare word must be classified by the router (and answered by the answering seat), \
             never pattern-matched into a direct execution: {}",
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
        let (mut writer, mut reader) = spawn_test_agent(router, never_answer);
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

    /// (#1684 remainder — cancellation) `session/cancel` actually aborts an
    /// in-flight no-slash-route command, and the `session/prompt` response
    /// reports `StopReason::Cancelled` — the protocol-level contract this
    /// packet's own audit found completely unhandled (`session/cancel` was
    /// a documented no-op). The canned router blocks on a channel recv
    /// rather than a fixed sleep, and signals `started_tx` the instant it
    /// starts running — proving this test cancels REAL in-flight work
    /// (never a command that raced ahead and finished first, which would
    /// otherwise show up as a silently-green false pass).
    #[tokio::test]
    #[serial_test::serial]
    async fn session_cancel_aborts_an_in_flight_command_and_reports_cancelled() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        write_echo_fixture(crew_tmp.path(), "echo-fixture", "fixture output");

        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let started_tx = std::sync::Mutex::new(Some(started_tx));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let router = move |_msg: &str| -> Result<String> {
            if let Some(tx) = started_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            // Blocks until the test explicitly releases it (or the test
            // itself ends and drops `release_tx`) — never actually reached
            // by a correctly-cancelled task; only here so a REGRESSION
            // (cancellation stops working) hangs this call instead of
            // racing ahead and returning before the cancel could land.
            let _ = release_rx.lock().unwrap().recv_timeout(std::time::Duration::from_secs(10));
            Ok("```json\n{\"command\": \"echo-fixture\", \"args\": \"\"}\n```".to_string())
        };
        let (mut writer, mut reader) = spawn_test_agent(router, never_answer);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        send_prompt(&mut writer, &session_id, "please give me the fixture").await;

        // Wait until the router closure has actually started running before
        // cancelling — see this test's own doc. Off on its OWN
        // `spawn_blocking` thread, never a bare synchronous `recv_timeout`
        // on the test's own async task: this test runs under the default
        // (single-threaded) `#[tokio::test]` flavor, so blocking that one
        // worker thread directly would starve the very `serve()` tasks
        // (including the router's own `spawn_blocking` closure) this wait
        // depends on — a self-deadlock, confirmed live (this test hung on
        // its first draft until switched to this shape).
        tokio::task::spawn_blocking(move || started_rx.recv_timeout(std::time::Duration::from_secs(5)))
            .await
            .expect("joining the started-signal wait")
            .expect("the router must start running before the test can cancel it");

        send_json(
            &mut writer,
            serde_json::json!({
                "jsonrpc": "2.0", "method": "session/cancel",
                "params": {"sessionId": session_id}
            }),
        )
        .await;

        let cancelled_chunk = recv_json(&mut reader).await;
        assert!(
            chunk_text(&cancelled_chunk).contains("cancelled"),
            "{}",
            chunk_text(&cancelled_chunk)
        );

        let final_response = recv_json(&mut reader).await;
        assert_eq!(
            final_response["result"]["stopReason"], "cancelled",
            "a cancelled in-flight command must report StopReason::Cancelled: {final_response}"
        );

        drop(release_tx); // let the detached router closure unblock and exit cleanly
    }

    /// (#1684 remainder) `session/cancel` for a session with nothing in
    /// flight (already finished, or an id this process never minted) is a
    /// quiet no-op — the connection must stay healthy and keep serving
    /// ordinary prompts afterward, proving this handler never poisons the
    /// dispatch loop.
    #[tokio::test]
    #[serial_test::serial]
    async fn session_cancel_for_unknown_session_is_a_quiet_no_op() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        write_echo_fixture(crew_tmp.path(), "echo-fixture", "fixture output");

        let router = |_msg: &str| -> Result<String> {
            panic!("the slash-command path must never invoke the router");
        };
        let (mut writer, mut reader) = spawn_test_agent(router, never_answer);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        send_json(
            &mut writer,
            serde_json::json!({
                "jsonrpc": "2.0", "method": "session/cancel",
                "params": {"sessionId": "darkmux-acp-does-not-exist"}
            }),
        )
        .await;

        // The connection must still be healthy: an ordinary slash command
        // on the REAL session id executes normally afterward.
        send_prompt(&mut writer, &session_id, "/echo-fixture").await;
        let output = recv_json(&mut reader).await;
        assert_eq!(chunk_text(&output), "fixture output");
        let final_response = recv_json(&mut reader).await;
        assert_end_turn(&final_response);
    }

    /// (#1684 remainder — session hygiene) `session/close` is the map-
    /// pruning mechanism the module doc's own "never pruned" finding named
    /// — proven observably (no internal test hook needed): a prompt sent on
    /// the SAME session id after closing it must see its `cwd` entry gone,
    /// exactly as if that session id had never been minted by `session/new`
    /// at all.
    #[tokio::test]
    #[serial_test::serial]
    async fn session_close_prunes_the_session_and_a_later_prompt_finds_no_cwd() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        write_echo_fixture(crew_tmp.path(), "echo-fixture", "fixture output");

        let router = |_msg: &str| -> Result<String> {
            panic!("the slash-command path must never invoke the router");
        };
        let (mut writer, mut reader) = spawn_test_agent(router, never_answer);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        // Sanity: the session works before closing.
        send_prompt(&mut writer, &session_id, "/echo-fixture").await;
        let output = recv_json(&mut reader).await;
        assert_eq!(chunk_text(&output), "fixture output");
        let before_close_response = recv_json(&mut reader).await;
        assert_end_turn(&before_close_response);

        send_json(
            &mut writer,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 99, "method": "session/close",
                "params": {"sessionId": session_id}
            }),
        )
        .await;
        let close_response = recv_json(&mut reader).await;
        assert!(close_response.get("result").is_some(), "session/close must succeed: {close_response}");

        // The session's entry is gone — the SAME session id now behaves
        // exactly like one that was never minted.
        send_prompt(&mut writer, &session_id, "/echo-fixture").await;
        let no_cwd = recv_json(&mut reader).await;
        assert!(
            chunk_text(&no_cwd).contains("no working directory recorded"),
            "a prompt on a CLOSED session must find its cwd entry pruned: {}",
            chunk_text(&no_cwd)
        );
    }

    /// (#1781) The set-site test the unit tests structurally cannot be:
    /// `should_idle_exit`'s own tests construct an `IdleState` by hand, so
    /// they would all stay green if `session/new` simply stopped latching
    /// it. This drives the REAL handler chain over the wire and asserts the
    /// flag on the state `serve()` is actually using.
    ///
    /// It also pins the reported bug's second path end to end: close the
    /// only session — which empties the `sessions` map, the predicate this
    /// fix replaced — and the process must STILL not be reclaimable at the
    /// configured window, because the latch is never cleared.
    #[tokio::test]
    #[serial_test::serial]
    async fn session_new_latches_ever_attached_and_close_never_clears_it() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());

        let router = |_msg: &str| -> Result<String> {
            panic!("this scenario never prompts, so the router must never be reached");
        };
        let (mut writer, mut reader, idle) = spawn_test_agent_observing_idle(router, never_answer);

        assert!(
            !idle.session_ever_attached.load(Ordering::SeqCst),
            "a freshly served process has had nothing attached to it yet"
        );

        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;
        assert!(
            idle.session_ever_attached.load(Ordering::SeqCst),
            "session/new must latch the ever-attached flag"
        );

        send_json(
            &mut writer,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 99, "method": "session/close",
                "params": {"sessionId": session_id}
            }),
        )
        .await;
        let close_response = recv_json(&mut reader).await;
        assert!(close_response.get("result").is_some(), "session/close must succeed: {close_response}");

        assert!(
            idle.session_ever_attached.load(Ordering::SeqCst),
            "session/close prunes the sessions map but must never clear the latch — that is \
             exactly the path #1781 would recur on"
        );
        idle.last_activity.store(0, Ordering::SeqCst);
        assert!(
            !should_idle_exit(&idle, 3600, 1800),
            "an hour after closing its only session, this process must still not self-exit at \
             the configured window"
        );
    }

    /// (#1781) The LOOP itself, run for real against the real `IdleState` a
    /// served connection is wired to — the assertion `should_idle_exit`'s
    /// own unit tests structurally cannot make. Those hand-build a state, so
    /// they all stay green if the loop's call site stops passing the state
    /// it actually has; a review proved exactly that revert
    /// (`should_idle_exit(&IdleState::new(), ..)`) built clean, passed
    /// `clippy -D warnings` and left the whole suite green while defeating
    /// the entire fix. `idle_self_exit_loop_with` exists so this test can
    /// exercise that call site with a 1ms tick instead of the production 60s
    /// one.
    ///
    /// Two halves, and BOTH are load-bearing. The negative half is the fix
    /// (an attached process is not reclaimed even at a 0-second configured
    /// window, because the ceiling governs it). The positive half is the
    /// control that keeps the negative one from passing vacuously — the
    /// same loop, same tick, same threshold, against a state nothing ever
    /// attached to, MUST reclaim.
    #[tokio::test]
    #[serial_test::serial]
    async fn the_real_loop_never_reclaims_an_attached_process() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());

        let router = |_msg: &str| -> Result<String> {
            panic!("this scenario never prompts, so the router must never be reached");
        };
        let (mut writer, mut reader, idle) = spawn_test_agent_observing_idle(router, never_answer);
        let cwd = std::env::temp_dir();
        let _session_id = handshake(&mut writer, &mut reader, &cwd).await;

        // A 0-second configured window is the most hostile setting the
        // orphan tier can have: every tick sees `idle_for >= 0`. The
        // attached tier must still be governed by `hard_idle_threshold(0)`
        // — the week-scale floor — so this loop must never fire.
        let exits = Arc::new(AtomicUsize::new(0));
        let exits_for_loop = exits.clone();
        let attached_loop = idle_self_exit_loop_with(
            idle.clone(),
            0,
            std::time::Duration::from_millis(1),
            move |_message| {
                exits_for_loop.fetch_add(1, AtomicOrdering::SeqCst);
            },
        );
        let outcome = tokio::time::timeout(std::time::Duration::from_millis(250), attached_loop).await;
        assert!(
            outcome.is_err(),
            "the loop must keep running against an attached process, never decide to exit"
        );
        assert_eq!(
            exits.load(AtomicOrdering::SeqCst),
            0,
            "a process a client attached a session to must never be reclaimed by the idle loop, \
             however short the configured window"
        );

        // The control: the SAME loop, tick and threshold, against a state
        // nothing ever attached to. If this doesn't fire, the assertion
        // above proves nothing.
        let orphan = Arc::new(IdleState::new());
        let orphan_exits = Arc::new(AtomicUsize::new(0));
        let orphan_exits_for_loop = orphan_exits.clone();
        let orphan_loop = idle_self_exit_loop_with(
            orphan,
            0,
            std::time::Duration::from_millis(1),
            move |_message| {
                orphan_exits_for_loop.fetch_add(1, AtomicOrdering::SeqCst);
            },
        );
        tokio::time::timeout(std::time::Duration::from_millis(250), orphan_loop)
            .await
            .expect("the loop must reclaim a process no session ever attached to");
        assert_eq!(
            orphan_exits.load(AtomicOrdering::SeqCst),
            1,
            "the orphan control must have exited exactly once"
        );
    }

    /// (#1781) EVERY handler that sees client traffic stamps
    /// `last_activity` — asserted per handler, over the wire, against the
    /// real state `serve()` is using.
    ///
    /// Without this, all four stamps are deletable with the whole suite
    /// green, and the production failure is invisible to CI forever: with
    /// them gone `last_activity` freezes at session attach, so the hard
    /// ceiling counts from ATTACH rather than from the client's last byte,
    /// and a panel in continuous daily use on an always-on machine dies at
    /// exactly 7 days of uptime — #1781 recurring on a 7-day fuse.
    ///
    /// Each handler is driven after storing a SENTINEL, and the assertion
    /// is that the sentinel is gone. Not "the value grew": `elapsed_secs`
    /// has 1-second granularity and this whole test runs in milliseconds,
    /// so a real stamp writes ~0. Overwriting the sentinel is the
    /// observable fact, and it is exactly the one a deleted stamp loses.
    ///
    /// The prompt half sends WHITESPACE deliberately. A prompt that
    /// executes also stamps on completion (`IdleState::command_finished`),
    /// which would mask the receipt-time stamp; the empty-text arm returns
    /// without ever spawning a command, so only the handler's own stamp can
    /// clear the sentinel.
    #[tokio::test]
    #[serial_test::serial]
    async fn every_client_facing_handler_stamps_activity() {
        const SENTINEL: u64 = 9_999_999;

        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());

        let router = |_msg: &str| -> Result<String> {
            panic!("this scenario never routes, so the router must never be reached");
        };
        let (mut writer, mut reader, idle) = spawn_test_agent_observing_idle(router, never_answer);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        // session/prompt — the whitespace arm, so no command spawns and
        // `command_finished`'s own stamp can't stand in for this one.
        idle.last_activity.store(SENTINEL, Ordering::SeqCst);
        send_prompt(&mut writer, &session_id, "   ").await;
        let _not_a_command = recv_json(&mut reader).await;
        let prompt_response = recv_json(&mut reader).await;
        assert_end_turn(&prompt_response);
        assert_ne!(
            idle.last_activity.load(Ordering::SeqCst),
            SENTINEL,
            "session/prompt must stamp activity on receipt"
        );

        // session/set_config_option — adjusting a picker is somebody at the
        // keyboard.
        idle.last_activity.store(SENTINEL, Ordering::SeqCst);
        send_json(
            &mut writer,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 4, "method": "session/set_config_option",
                "params": {"sessionId": session_id, "configId": "humor", "value": "90"}
            }),
        )
        .await;
        let set_response = recv_json(&mut reader).await;
        assert!(set_response.get("result").is_some(), "{set_response}");
        assert_ne!(
            idle.last_activity.load(Ordering::SeqCst),
            SENTINEL,
            "session/set_config_option must stamp activity"
        );

        // session/cancel — a notification, so there is no response to wait
        // on; poll the stamp instead rather than assuming the handler has
        // already run.
        idle.last_activity.store(SENTINEL, Ordering::SeqCst);
        send_json(
            &mut writer,
            serde_json::json!({
                "jsonrpc": "2.0", "method": "session/cancel",
                "params": {"sessionId": session_id}
            }),
        )
        .await;
        assert!(
            await_stamp(&idle, SENTINEL).await,
            "session/cancel must stamp activity — a stop-button press is client traffic"
        );

        // session/close — closing a thread is client traffic too, and the
        // stamp is what keeps the hard ceiling counting from the close
        // rather than from before it.
        idle.last_activity.store(SENTINEL, Ordering::SeqCst);
        send_json(
            &mut writer,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 5, "method": "session/close",
                "params": {"sessionId": session_id}
            }),
        )
        .await;
        let close_response = recv_json(&mut reader).await;
        assert!(close_response.get("result").is_some(), "{close_response}");
        assert_ne!(
            idle.last_activity.load(Ordering::SeqCst),
            SENTINEL,
            "session/close must stamp activity"
        );
    }

    /// Wait (bounded) for `last_activity` to stop being `sentinel` —
    /// the synchronization a NOTIFICATION handler needs, since it sends no
    /// response a test could await. Returns whether the stamp landed.
    async fn await_stamp(idle: &Arc<IdleState>, sentinel: u64) -> bool {
        for _ in 0..400 {
            if idle.last_activity.load(Ordering::SeqCst) != sentinel {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        false
    }

    /// (#1781) `session/load` — a resume against a process that never
    /// minted the id itself (the binary-swap case scope G1 exists for) —
    /// attaches a session just as `session/new` does, and must latch the
    /// same flag. Without this, a resumed panel is one that "never had a
    /// session attached" as far as the backstop is concerned.
    #[tokio::test]
    #[serial_test::serial]
    async fn session_load_latches_ever_attached() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());

        let router = |_msg: &str| -> Result<String> {
            panic!("this scenario never prompts, so the router must never be reached");
        };
        let (mut writer, mut reader, idle) = spawn_test_agent_observing_idle(router, never_answer);

        send_json(
            &mut writer,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": 1, "clientCapabilities": {}}
            }),
        )
        .await;
        let _init_response = recv_json(&mut reader).await;
        assert!(
            !idle.session_ever_attached.load(Ordering::SeqCst),
            "initialize alone attaches no session"
        );

        let cwd = std::env::temp_dir();
        send_json(
            &mut writer,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/load",
                "params": {"sessionId": "darkmux-acp-from-a-previous-process", "cwd": cwd.to_string_lossy(), "mcpServers": []}
            }),
        )
        .await;
        let load_response = recv_json(&mut reader).await;
        assert!(load_response.get("result").is_some(), "session/load must succeed: {load_response}");
        let _available_commands_update = recv_json(&mut reader).await;

        assert!(
            idle.session_ever_attached.load(Ordering::SeqCst),
            "session/load must latch the ever-attached flag"
        );
    }

    /// (#1777 merge gate — test gap) Every EXISTING `session/close` test
    /// closes an IDLE session; none of them exercise the "close implies
    /// cancel first" branch the module doc itself claims
    /// (`session/close`'s own comment: "Per spec, close implies cancel
    /// first"). This proves it: closing a session with a REAL in-flight
    /// no-slash-route command must (a) respond to the close request
    /// PROMPTLY — before the still-blocked router closure ever releases,
    /// proving close does not wait on the work it's aborting — and (b)
    /// still let the aborted command's own deferred "cancelled" chunk +
    /// `StopReason::Cancelled` response land afterward, exactly like
    /// `session_cancel_aborts_an_in_flight_command_and_reports_cancelled`
    /// proves for `session/cancel` — since both notifications drive the
    /// SAME `InFlight` abort path.
    #[tokio::test]
    #[serial_test::serial]
    async fn session_close_aborts_an_in_flight_command_before_pruning_the_session() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        write_echo_fixture(crew_tmp.path(), "echo-fixture", "fixture output");

        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let started_tx = std::sync::Mutex::new(Some(started_tx));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let router = move |_msg: &str| -> Result<String> {
            if let Some(tx) = started_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            // Blocks until the test releases it (or drops `release_tx`) —
            // see `session_cancel_aborts_an_in_flight_command_and_reports_
            // cancelled`'s own doc on why this shape proves REAL in-flight
            // work gets cancelled, not a command that raced ahead.
            let _ = release_rx.lock().unwrap().recv_timeout(std::time::Duration::from_secs(10));
            Ok("```json\n{\"command\": \"echo-fixture\", \"args\": \"\"}\n```".to_string())
        };
        let (mut writer, mut reader) = spawn_test_agent(router, never_answer);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        send_prompt(&mut writer, &session_id, "please give me the fixture").await;

        // Same "wait on its own spawn_blocking thread" shape as the
        // session/cancel test — see that test's own doc on why a bare
        // synchronous wait on this (single-threaded) test's own task would
        // self-deadlock.
        tokio::task::spawn_blocking(move || started_rx.recv_timeout(std::time::Duration::from_secs(5)))
            .await
            .expect("joining the started-signal wait")
            .expect("the router must start running before the test can close its session");

        send_json(
            &mut writer,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 99, "method": "session/close",
                "params": {"sessionId": session_id}
            }),
        )
        .await;

        // The close response must arrive WITHOUT waiting for the still-
        // blocked router closure — proving `session/close` genuinely
        // aborted the in-flight command rather than awaiting it.
        let close_response = recv_json(&mut reader).await;
        assert_eq!(close_response["id"], 99, "expected the session/close response first: {close_response}");
        assert!(close_response.get("result").is_some(), "session/close must succeed: {close_response}");

        // The aborted command's own deferred reporting still lands.
        let cancelled_chunk = recv_json(&mut reader).await;
        assert!(
            chunk_text(&cancelled_chunk).contains("cancelled"),
            "{}",
            chunk_text(&cancelled_chunk)
        );
        let final_response = recv_json(&mut reader).await;
        assert_eq!(
            final_response["result"]["stopReason"], "cancelled",
            "session/close must abort the in-flight command, reporting StopReason::Cancelled: {final_response}"
        );

        drop(release_tx); // let the detached router closure unblock and exit cleanly
    }

    /// (#1777 merge gate — MUST FIX 1 tier 2) The "no OS subprocess to
    /// leak" claim was FALSE for the ephemeral `procedural.shell` runner —
    /// see the module doc's own correction. This proves the mitigation:
    /// cancelling a `procedural.shell` command still eventually reports
    /// what the (unkillable, `spawn_blocking`-bound) subprocess actually
    /// did, instead of throwing the result away. The shell step touches a
    /// marker file the instant it starts (proving the test cancels REAL
    /// in-flight work, the same discipline the router-based cancel tests
    /// use), sleeps briefly, then echoes a distinctive string that must
    /// show up in a LATE "completed after cancellation: ..." chunk sent
    /// well after the `session/prompt` response has already resolved
    /// `StopReason::Cancelled`.
    #[tokio::test]
    #[serial_test::serial]
    async fn ephemeral_command_cancellation_still_reports_the_shells_eventual_result() {
        let crew_tmp = tempfile::TempDir::new().unwrap();
        let _crew_guard = EnvGuard::set("DARKMUX_CREW_DIR", crew_tmp.path());
        let flows_tmp = tempfile::TempDir::new().unwrap();
        let _flows_guard = EnvGuard::set("DARKMUX_FLOWS_DIR", flows_tmp.path());

        let marker = crew_tmp.path().join("shell-started.marker");
        write_slow_shell_fixture(crew_tmp.path(), "slow-echo", &marker, 1, "slow-output-marker");

        let router = |_msg: &str| -> Result<String> {
            panic!("the slash-command path must never invoke the router");
        };
        let (mut writer, mut reader) = spawn_test_agent(router, never_answer);
        let cwd = std::env::temp_dir();
        let session_id = handshake(&mut writer, &mut reader, &cwd).await;

        send_prompt(&mut writer, &session_id, "/slow-echo").await;

        // Wait for the REAL shell subprocess to actually start (the
        // marker file appears) before cancelling.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !marker.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the shell step must start running before the test can cancel it");

        send_json(
            &mut writer,
            serde_json::json!({
                "jsonrpc": "2.0", "method": "session/cancel",
                "params": {"sessionId": session_id}
            }),
        )
        .await;

        let cancelled_chunk = recv_json(&mut reader).await;
        assert!(chunk_text(&cancelled_chunk).contains("cancelled"), "{}", chunk_text(&cancelled_chunk));

        let final_response = recv_json(&mut reader).await;
        assert_eq!(
            final_response["result"]["stopReason"], "cancelled",
            "a cancelled ephemeral command must still report StopReason::Cancelled promptly: {final_response}"
        );

        // The shell's `sleep` finishes on its OWN thread regardless of the
        // cancel above (it cannot be preempted) — its eventual result must
        // still land as a chunk, not vanish into a dropped JoinHandle.
        let completed_after_cancel =
            tokio::time::timeout(std::time::Duration::from_secs(5), recv_json(&mut reader))
                .await
                .expect(
                    "the detached watcher must still post the shell step's eventual result — \
                     MUST FIX 1 tier 2 regressed if this times out",
                );
        let text = chunk_text(&completed_after_cancel);
        assert!(text.contains("completed after cancellation"), "{text}");
        assert!(text.contains("slow-output-marker"), "{text}");
    }

    /// (#1777 merge gate — the headline mechanism, previously untested)
    /// The existing cancel tests
    /// (`session_cancel_aborts_an_in_flight_command_and_reports_cancelled`,
    /// the `session/close` sibling above) only prove the WIRE contract —
    /// `StopReason::Cancelled` comes back promptly — over the no-slash
    /// ROUTER path, which is precisely the path where nothing is
    /// killable (a plain synchronous call, no `Child` anywhere). The
    /// actual promise `kill_on_drop(true)` makes for `run_launch_command`'s
    /// real subprocess `Command`s — that the OS PROCESS itself dies, not
    /// just that the Rust future resolves — has rested on drop-topology
    /// reasoning alone until now.
    ///
    /// This proves that half directly: `tokio::spawn` a task that spawns
    /// a real, observably long-lived child (`sleep 30`, with
    /// `.kill_on_drop(true)` — the IDENTICAL flag `run_launch_command`
    /// sets on its own `Command`) and holds it
    /// across an in-place `.await` on `child.wait()` — the SAME "own the
    /// `Child` across an await point, get a real abort handle from
    /// `tokio::spawn`" shape those two functions use, and the exact shape
    /// `run_cancellable` wraps every `session/prompt` branch in. Aborting
    /// that task must leave the process GONE, polled via `kill -0 <pid>`
    /// (fails once the process is reaped) rather than trusted from
    /// reasoning about `Drop` order.
    ///
    /// RED-proved by hand: removing `.kill_on_drop(true)` from the
    /// `Command` below makes this test fail (timeout waiting for the
    /// process to disappear, since a plain `sleep 30` outlives the
    /// aborted task) — confirming the assertion actually exercises the
    /// flag, not just the task's own bookkeeping.
    #[tokio::test]
    async fn cancelling_a_task_holding_a_child_actually_kills_the_os_process() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("30");
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn().expect("spawning `sleep 30`");
        let pid = child.id().expect("a freshly spawned child has a pid");

        assert!(process_is_alive(pid), "the child must be running before cancellation");

        let handle = tokio::spawn(async move {
            let _ = child.wait().await;
        });
        // Let the spawned task actually get polled at least once before
        // aborting it — otherwise this could abort a task that was never
        // even scheduled yet, proving nothing about `kill_on_drop`.
        tokio::task::yield_now().await;

        handle.abort();
        let _ = handle.await;

        // `kill_on_drop`'s SIGKILL isn't necessarily synchronous with the
        // abort — poll with a bound rather than checking exactly once.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if !process_is_alive(pid) {
                return; // PASS — the OS process is genuinely gone.
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "process {pid} (`sleep 30`) is still alive 5s after aborting the task holding \
                     its Child — kill_on_drop did not terminate it"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// `kill -0 <pid>` — exits 0 iff a process with that pid exists and is
    /// signalable by this user; used only to observe the REAL OS process
    /// state in [`cancelling_a_task_holding_a_child_actually_kills_the_os_process`],
    /// never to affect it.
    fn process_is_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// (#1777 merge gate — CONSIDER, the lost-cancel race) A `session/
    /// cancel` that arrives BEFORE `run_cancellable` has registered its
    /// own handle used to be silently lost — see `InFlightSlot`'s own
    /// doc. This proves the fix at the exact seam that matters:
    /// `register_or_consume_cancel_tombstone` finds a pre-existing
    /// `Cancelled` tombstone (simulating the race deterministically,
    /// rather than trying to win a real timing race against the tokio
    /// scheduler) and reports "already cancelled" instead of registering
    /// a handle nobody will ever call `abort()` on.
    #[tokio::test]
    async fn a_cancel_tombstone_recorded_before_registration_is_consumed_and_reported() {
        let in_flight: InFlight = Arc::new(Mutex::new(HashMap::new()));
        let session_id = SessionId::new("darkmux-acp-test-session");

        // Simulate `session/cancel` racing ahead of the command's own
        // registration — exactly the window `InFlightSlot`'s doc names.
        in_flight.lock().unwrap().insert(session_id.clone(), InFlightSlot::Cancelled);

        let placeholder = tokio::spawn(async {});
        let already_cancelled =
            register_or_consume_cancel_tombstone(&in_flight, &session_id, placeholder.abort_handle());
        placeholder.abort();

        assert!(already_cancelled, "a pre-existing tombstone must be reported as already-cancelled");
        assert!(
            in_flight.lock().unwrap().get(&session_id).is_none(),
            "the tombstone must be CONSUMED (removed), not left in place to fire twice"
        );
    }

    /// (#1777 merge gate — CONSIDER, the lost-cancel race) The ordinary
    /// case: no tombstone waiting, so `register_or_consume_cancel_
    /// tombstone` registers the handle normally and reports "not yet
    /// cancelled" — the SAME behavior `run_cancellable` relied on before
    /// this fix, proving the race fix didn't change the common path.
    #[tokio::test]
    async fn no_tombstone_present_registers_the_handle_as_running() {
        let in_flight: InFlight = Arc::new(Mutex::new(HashMap::new()));
        let session_id = SessionId::new("darkmux-acp-test-session-2");

        let placeholder = tokio::spawn(async {});
        let already_cancelled =
            register_or_consume_cancel_tombstone(&in_flight, &session_id, placeholder.abort_handle());
        placeholder.abort();

        assert!(!already_cancelled, "with nothing tombstoned, the handle must register as the running command");
        assert!(
            matches!(in_flight.lock().unwrap().get(&session_id), Some(InFlightSlot::Running(_))),
            "the session's slot must now be Running"
        );
    }

    /// (#2476) `spawn_registered` must register its child's pid BEFORE
    /// the wait can observe it — proven by starting a real, long-lived
    /// `sleep 30` through it, then reaching that SAME pid through
    /// `darkmux_types::child_registry::kill_all` (the exact call
    /// `host_shutdown_reap_loop`'s production closure makes) and
    /// confirming the process actually died from it, not from its own
    /// `.kill_on_drop(true)` (also set, matching `run_launch_command`'s
    /// real `Command`, but this test wants to see `kill_all` do the
    /// killing — see the timing note below for why that's the thing
    /// under test).
    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_registered_pid_is_reachable_via_child_registry_kill_all() {
        darkmux_types::child_registry::reset_for_test();

        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("30");
        cmd.kill_on_drop(true);

        let handle = tokio::spawn(spawn_registered(cmd));
        // Give the spawned task a chance to actually reach `cmd.spawn()`
        // (and register) before `kill_all` runs — otherwise this could
        // kill nothing and prove nothing.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        darkmux_types::child_registry::kill_all(darkmux_types::child_registry::SIGKILL);

        let output = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("spawn_registered's task did not resolve within 5s of kill_all")
            .expect("joining spawn_registered's task")
            .expect("sleep 30 should spawn cleanly");
        assert!(
            !output.status.success(),
            "a child reached through child_registry::kill_all must not report success: {:?}",
            output.status
        );

        darkmux_types::child_registry::reset_for_test();
    }

    /// (#2476 review round 2, MUST FIX 1) `session/cancel`'s and
    /// `session/close`'s abort path drops `run_launch_command`'s awaited
    /// future AT its suspended `.await` inside `spawn_registered` — the
    /// ORIGINAL `spawn_registered` deregistered via a plain statement
    /// placed AFTER that await, which an abort never reaches, leaking the
    /// pid in a process-wide registry forever (the pid becomes recyclable
    /// the moment `kill_on_drop` reaps the underlying process, per
    /// `child_registry::kill_pid`'s own doc — a long-lived host signaling
    /// that stale entry later could hit an unrelated process). This
    /// proves the RAII fix directly: abort a task mid-`spawn_registered`
    /// the SAME way `handle.abort()` does at both real call sites
    /// (`session/cancel`, `session/close`), and show the pid is gone from
    /// `LAUNCH_CHILDREN` afterward — the bookkeeping cleaned up, not just
    /// the OS process (that half is `kill_on_drop`, already proven by
    /// `cancelling_a_task_holding_a_child_actually_kills_the_os_process`,
    /// above).
    ///
    /// RED-proved by hand: reverting `spawn_registered` to the pre-fix
    /// plain register/deregister pair (deregister placed after the
    /// awaited `wait_with_output()`, as it read before this fix) makes
    /// this test fail — the pid stays in `LAUNCH_CHILDREN` after the
    /// abort, since the statement that would have removed it is never
    /// reached.
    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_registered_deregisters_on_cancel_abort_not_just_on_completion() {
        darkmux_types::child_registry::reset_for_test();
        LAUNCH_CHILDREN.lock().unwrap().clear();

        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("30");
        cmd.kill_on_drop(true);

        let handle = tokio::spawn(spawn_registered(cmd));
        // Let the task actually reach `cmd.spawn()` (and register) before
        // aborting it — otherwise this could abort a task that never got
        // that far, proving nothing about the fix.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !LAUNCH_CHILDREN.lock().unwrap().is_empty(),
            "the spawned child must have registered before this test aborts its task"
        );

        handle.abort();
        let _ = handle.await;

        assert!(
            LAUNCH_CHILDREN.lock().unwrap().is_empty(),
            "an aborted spawn_registered task must deregister its pid via Drop — a plain \
             register/deregister pair (deregister placed after the awaited wait) never \
             reaches that statement when the future is dropped mid-await, leaking the pid"
        );

        darkmux_types::child_registry::reset_for_test();
    }

    /// (#2476, ready-latch added in review round 2 — CONSIDER 7)
    /// `wait_for_host_shutdown_signal_ready` must react to a REAL
    /// SIGTERM, not a structural assertion — sends this test's OWN process a real
    /// `kill -TERM`, the same technique `launch_guard.rs`'s own
    /// `arm_installs_real_sigterm_and_sighup_handlers` test uses to prove
    /// ITS handler actually fires, and asserts the future resolves within
    /// a bound rather than hanging.
    ///
    /// Waits on [`wait_for_host_shutdown_signal_ready`]'s readiness latch
    /// rather than a fixed sleep before sending the signal — a sleep bets
    /// the listener registered in time; on a loaded box that bet can
    /// lose, and an unregistered `SIGTERM` takes default disposition,
    /// which kills the whole test BINARY, not just this test.
    #[tokio::test]
    #[serial_test::serial]
    async fn wait_for_host_shutdown_signal_resolves_on_a_real_sigterm() {
        let pid = std::process::id().to_string();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let wait = tokio::spawn(wait_for_host_shutdown_signal_ready(Some(ready_tx)));
        tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx)
            .await
            .expect("the SIGTERM listener did not report ready within 5s")
            .expect("the ready sender was dropped without firing");

        assert!(
            std::process::Command::new("kill").args(["-TERM", &pid]).status().expect("running kill -TERM").success(),
            "kill -TERM itself must succeed sending a real signal to this process"
        );

        tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .expect("wait_for_host_shutdown_signal_ready did not resolve within 5s of a real SIGTERM")
            .expect("joining the wait task");
    }

    /// (#2476) `host_shutdown_reap_loop` must call `on_signal` — and ONLY
    /// after a real signal arrives, never eagerly. Proven with an
    /// injected callback (an `AtomicBool`, never `std::process::exit`)
    /// against the same real self-SIGTERM technique as the test above —
    /// this is the piece that proves the WIRING between signal detection
    /// and the reap action. The reap action's own production closure
    /// (`mark_interrupted` + `kill_all` + `std::process::exit`) is NOT
    /// exercised end-to-end here: `std::process::exit` cannot be run
    /// in-process without killing the test binary, the same reason
    /// `launch_guard.rs`'s `reap_and_exit_on_signal` is never unit-tested
    /// directly either — only `mark_interrupted` (unit-tested in
    /// `darkmux_types::interrupt`) and `kill_all` (real-child-death proved
    /// by `spawn_registered_pid_is_reachable_via_child_registry_kill_all`,
    /// above) are individually proven; this test proves the glue that
    /// calls them actually runs when the real signal lands.
    #[tokio::test]
    #[serial_test::serial]
    async fn host_shutdown_reap_loop_calls_on_signal_only_after_a_real_signal() {
        let called = Arc::new(AtomicBool::new(false));
        let called_for_closure = called.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(host_shutdown_reap_loop_ready(
            move || {
                called_for_closure.store(true, AtomicOrdering::SeqCst);
            },
            Some(ready_tx),
        ));

        // (#2476 review round 2, CONSIDER 7) A latch, not a fixed sleep —
        // see `wait_for_host_shutdown_signal_ready`'s own doc for why an
        // unregistered listener is a whole-binary hazard, not just a
        // flaky assertion.
        tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx)
            .await
            .expect("the SIGTERM listener did not report ready within 5s")
            .expect("the ready sender was dropped without firing");
        assert!(!called.load(AtomicOrdering::SeqCst), "on_signal must not fire before any signal arrives");

        let pid = std::process::id().to_string();
        assert!(
            std::process::Command::new("kill").args(["-TERM", &pid]).status().expect("running kill -TERM").success(),
            "kill -TERM itself must succeed sending a real signal to this process"
        );

        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("host_shutdown_reap_loop did not resolve within 5s of a real SIGTERM")
            .expect("joining the loop task");

        assert!(called.load(AtomicOrdering::SeqCst), "on_signal must fire once the real signal is observed");
    }
}
