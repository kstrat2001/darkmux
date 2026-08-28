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
//!   Packet B2 scope G2) exits the whole process — map included — once
//!   nothing has been in flight for `acp_idle_exit_minutes`.
//! - (#1684 remainder — RESOLVED) Cancellation is wired. `session/cancel`
//!   (`CancelNotification`) looks up the session's in-flight command in the
//!   [`InFlight`] registry and calls `AbortHandle::abort()` on it. The
//!   command execution itself now runs as its OWN `tokio::spawn`'d task
//!   (registered in that map for the duration of `session/prompt`'s
//!   `cx.spawn`'d closure from Packet 2) rather than directly inline in that
//!   closure — `cx.spawn` alone never hands back an abort handle, so
//!   cancellation needed a genuinely abortable task underneath it. Because
//!   [`run_review`]'s and [`run_launch_command`]'s subprocess `Command`s set
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
//!   finalize step — a cancelled `run_review`/`run_launch_command` mission
//!   is left permanently `Active` (`darkmux mission status` will flag it;
//!   a manual `mission abort` reconciles it), and the temp diff file
//!   `run_review` writes is never cleaned up (`tokio::fs::remove_file`
//!   sits AFTER `child.wait().await`, a line a cancel never reaches).
//!   Neither is fixable by anything short of a completion-independent
//!   cleanup path; named here so a stop-button user isn't surprised by
//!   drift accumulating in `mission status` or stray files under the
//!   workspace.
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
//! - (#1684 remainder — RESOLVED) `run_review`'s stderr-draining loop used
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
    AgentCapabilities, AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, CancelNotification,
    CloseSessionRequest, CloseSessionResponse, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionCapabilities, SessionCloseCapabilities,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectOption,
    SessionId, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    StopReason, ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, UnstructuredCommandInput,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Stdio as AcpStdio};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio as ProcStdio;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
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

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// (#1698 Packet B2, scope G2) Background idle self-exit loop, spawned once
/// per `serve()` call. Checks every 60s (a check-cadence far below any
/// realistic idle threshold, so it never meaningfully delays the exit); on
/// a MINUTES-scale idle threshold this coarseness is a non-issue. Exits the
/// WHOLE PROCESS (`std::process::exit(0)`) — never returns an error, never
/// tears down the connection gracefully first, because there is nothing
/// left to tear down: zero sessions have done anything and zero commands
/// are running, by construction of the check itself. `acp_idle_exit_minutes
/// == 0` disables the loop entirely (checked once, up front — not on every
/// tick, so a `0` config never even starts the sleep loop).
async fn idle_self_exit_loop(last_activity_unix: Arc<AtomicU64>, in_flight: Arc<AtomicI64>) {
    let idle_minutes = darkmux_types::config_access::acp_idle_exit_minutes();
    if idle_minutes == 0 {
        return;
    }
    let idle_seconds = idle_minutes * 60;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        if in_flight.load(Ordering::SeqCst) > 0 {
            continue;
        }
        let idle_for = now_unix().saturating_sub(last_activity_unix.load(Ordering::SeqCst));
        if idle_for >= idle_seconds {
            eprintln!(
                "[darkmux-acp] idle for {idle_for}s (>= {idle_seconds}s configured) with zero \
                 commands in flight — self-exiting"
            );
            std::process::exit(0);
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
/// session's config-option overrides; `run()`'s production call site wires
/// `radio_answer::dispatch_answerer_call_with` directly (its signature
/// already matches this alias exactly).
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
    let answerer: AnswererCall = Arc::new(crate::radio_answer::dispatch_answerer_call_with);
    let scope: ScopeCall = Arc::new(crate::radio_answer::grounding_scope_for);
    rt.block_on(serve(router, AnsweringSeat { call: answerer, scope }, AcpStdio::new()))?;
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
    seat: AnsweringSeat,
    transport: impl agent_client_protocol::ConnectTo<Agent> + 'static,
) -> Result<()> {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let next_session_ordinal = Arc::new(AtomicU64::new(1));

    // (#1698 Packet B2, scope G2 — idle self-exit) `last_activity_unix`
    // updates on every `session/new` + `session/prompt`; `in_flight` counts
    // commands/routes currently executing. The background loop spawned
    // below self-exits the process once BOTH are quiet for
    // `acp_idle_exit_minutes` — "most swaps find no process running" (the
    // issue's session-hygiene addendum).
    let last_activity_unix = Arc::new(AtomicU64::new(now_unix()));
    let in_flight = Arc::new(AtomicI64::new(0));
    tokio::spawn(idle_self_exit_loop(last_activity_unix.clone(), in_flight.clone()));

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
    let activity_for_new = last_activity_unix.clone();
    let activity_for_prompt = last_activity_unix.clone();
    let activity_for_load = last_activity_unix.clone();
    let activity_for_config = last_activity_unix.clone();
    let in_flight_for_prompt = in_flight.clone();
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
                activity_for_new.store(now_unix(), Ordering::SeqCst);
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
                activity_for_prompt.store(now_unix(), Ordering::SeqCst);
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
                    let in_flight_for_task = in_flight_for_prompt.clone();
                    let in_flight_tasks_for_task = in_flight_tasks_for_prompt.clone();
                    let activity_for_task = activity_for_prompt.clone();
                    return cx.spawn(async move {
                        // (#1698 Packet B2, scope G2) Incremented HERE, as
                        // the future's own first action, not before
                        // `cx.spawn` — a `cx.spawn` call that itself returns
                        // `Err` (scheduling failure) never runs this body at
                        // all, so incrementing before the call would leak a
                        // count nothing ever decrements, disabling idle
                        // self-exit for the rest of the process's life.
                        in_flight_for_task.fetch_add(1, Ordering::SeqCst);
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
                        // Stamped on COMPLETION, not just on receipt (the
                        // top of the handler) — otherwise a long-running
                        // command (a multi-minute `/review`) drops
                        // `in_flight` to 0 the instant it finishes while
                        // `last_activity_unix` still holds the RECEIPT
                        // timestamp from minutes ago, and the very next
                        // idle-loop tick sees a stale idle window and exits
                        // while the operator is still reading the result.
                        activity_for_task.store(now_unix(), Ordering::SeqCst);
                        in_flight_for_task.fetch_sub(1, Ordering::SeqCst);

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
                let in_flight_for_task = in_flight_for_prompt.clone();
                let in_flight_tasks_for_task = in_flight_tasks_for_prompt.clone();
                let activity_for_task = activity_for_prompt.clone();
                cx.spawn(async move {
                    // (#1698 Packet B2, scope G2) See the slash-path arm's
                    // own comment on why this increments HERE, inside the
                    // future, rather than before `cx.spawn`.
                    in_flight_for_task.fetch_add(1, Ordering::SeqCst);
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
                    // Stamped on completion — see the slash-path arm's own
                    // comment on why receipt-only stamping is wrong.
                    activity_for_task.store(now_unix(), Ordering::SeqCst);
                    in_flight_for_task.fetch_sub(1, Ordering::SeqCst);
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
                activity_for_load.store(now_unix(), Ordering::SeqCst);
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
                activity_for_config.store(now_unix(), Ordering::SeqCst);
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
    sessions: &Sessions,
) -> Result<()> {
    match plan {
        // `review` keeps its EXISTING bespoke path, byte-for-byte
        // unchanged (#1684 rule C) — only routed to differently.
        // `config_id` (#1695 merge-gate MUST FIX) is the
        // REGISTRY-RESOLVABLE id `route_command` decided on —
        // never a hardcoded `"review"` — so a panel-advertised
        // review VARIANT launches itself, not the built-in.
        crate::acp_panel::RoutePlan::Review(config_id) => {
            run_review(session_id, &config_id, args, cwd, cx, sessions).await
        }
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
        crate::radio_answer::answer(&text_owned, &catalog, &shelf, &cwd_owned, scope, &mut |m: &str| {
            (seat.call)(m, &overrides)
        })
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
async fn run_review(
    session_id: &SessionId,
    config_id: &str,
    args: &str,
    cwd: &Path,
    cx: &ConnectionTo<Client>,
    sessions: &Sessions,
) -> Result<()> {
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
        // (#1684 remainder — cancellation) A `session/cancel` aborts the
        // `tokio::spawn`'d task awaiting this child (see `run_cancellable`),
        // which drops `child` mid-`.wait()`. `kill_on_drop` is what turns
        // that drop into a real SIGKILL on the OS process instead of an
        // orphan — see the module doc's "Cancellation is wired" note.
        .kill_on_drop(true)
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
    // (#1698 Packet B2, scope C) Shelve the rendered result BEFORE sending
    // it — the answering seat's own dispatch (if this session later asks a
    // question the router refuses) never races the shelf write against the
    // notification itself.
    session_shelf_push(sessions, session_id, crate::radio_answer::shelf_entry(config_id, args, &final_text));
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
    // `run_review`'s own comment on the same ordering.
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
    sessions: &Sessions,
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
        // (#1684 remainder — cancellation) See `run_review`'s own comment on
        // this same flag — same subprocess-kill-on-abort mechanism.
        .kill_on_drop(true)
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
    // (#1698 Packet B2, scope C) Shelved BEFORE the notification — see
    // `run_review`'s own comment on the same ordering.
    session_shelf_push(sessions, session_id, crate::radio_answer::shelf_entry(config_id, args, &text));
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
///
/// (#1684 — chunk-noise filter) Also drops darkmux-flow's sink-init
/// diagnostics (`crates/darkmux-flow/src/lib.rs::build_default_sink`),
/// which print UNCONDITIONALLY on stderr the first time ANY process touches
/// the flow crate — i.e. every `mission launch` subprocess this file spawns,
/// review or otherwise, every time. Observed live leaking into the Zed
/// panel as raw agent chunks (e.g. "flow: Redis sink enabled —
/// url=redis://... stream=darkmux:flow max_len=None (composed via
/// TeeSink)"). These lines are legitimate diagnostics for a terminal
/// operator — never silenced at the source, matching the module's own "why
/// stdout is off-limits" reasoning about not touching the emitter — but
/// infrastructure chatter with nothing to say to someone reading a chat
/// panel. Matched on the flow crate's own literal `"flow: "` prefix
/// convention, the ONLY startup-noise prefix in the tree confirmed to fire
/// unconditionally on a normal `mission launch` run (audited 2026-08-12:
/// the `warning:`/`radio:`/`funnels:`/`scores:`/`debates:`/`machine:`
/// prefixes elsewhere in the codebase are either error-path-only or belong
/// to CLI verbs this file never subprocesses into) — a narrow, conservative
/// match, never a broad heuristic that could eat real model output.
fn forwardable_chunk_text(line: &str) -> Option<String> {
    forwardable_chunk_text_with_record(line, |dropped| {
        eprintln!("[darkmux-acp] subprocess: {dropped}");
    })
}

/// [`forwardable_chunk_text`]'s actual logic, with the "what to do with a
/// line we're dropping from the chat" side effect factored out as an
/// injectable `record` callback (#1777 merge gate, MUST FIX 2) — the SAME
/// `Arc<dyn Fn>`-injection shape this file already uses for
/// `RouterCall`/`AnswererCall`/`ScopeCall` so the decision ("forward to
/// chat" vs "suppress but still record") stays unit-testable without
/// capturing this PROCESS'S real stderr (fd 2 is process-global; every
/// `cargo test` thread shares it, so redirecting it mid-test would be
/// flaky by construction). Production wraps this with a plain
/// `eprintln!("[darkmux-acp] subprocess: {line}")` — see
/// [`forwardable_chunk_text`] — so every filtered line still lands on
/// this process's own stderr (Zed's logs panel) even when it's not
/// narrated in the chat transcript: filter the CHAT, never the RECORD.
fn forwardable_chunk_text_with_record(line: &str, mut record: impl FnMut(&str)) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('{') || trimmed.starts_with("flow: ") {
        record(line);
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
        let (test_writer, agent_reader) = tokio::io::duplex(64 * 1024);
        let (agent_writer, test_reader) = tokio::io::duplex(64 * 1024);
        let router_call: RouterCall = Arc::new(router);
        let answerer_call: AnswererCall = Arc::new(answerer);
        // Pinned to `Full` (#1698 Packet B2 gate): these tests exercise the
        // WIRE, not the data boundary — see `ScopeCall`'s own doc.
        let scope_call: ScopeCall = Arc::new(|_| crate::radio_answer::GroundingScope::Full);
        let transport = ByteStreams::new(agent_writer.compat_write(), agent_reader.compat());
        tokio::spawn(async move {
            let _ = serve(router_call, AnsweringSeat { call: answerer_call, scope: scope_call }, transport).await;
        });
        (test_writer, BufReader::new(test_reader))
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

    /// (#1684 remainder — chunk-noise filter) darkmux-flow's sink-init
    /// diagnostics must never reach the ACP wire as agent chunks — the
    /// defect observed live (issue #1684's own "flow: ... composed via
    /// TeeSink" example). A real (non-noise) stderr line must still pass
    /// through untouched, proving the filter is narrow, not a blanket drop.
    #[test]
    fn forwardable_chunk_text_filters_flow_sink_startup_noise() {
        assert_eq!(
            forwardable_chunk_text(
                "flow: Redis sink enabled — url=redis://x stream=darkmux:flow max_len=None (composed via TeeSink)"
            ),
            None
        );
        assert_eq!(
            forwardable_chunk_text(
                "flow: AuditFileSink enabled — audit_dir=/tmp/x (hash-chained, flock-serialized)"
            ),
            None
        );
        assert_eq!(
            forwardable_chunk_text("some real progress line"),
            Some("`some real progress line`".to_string())
        );
    }

    /// (#1777 merge gate — MUST FIX 2) A dropped line is filtered from the
    /// CHAT, never from the RECORD. darkmux-flow's degraded-mode warnings
    /// (a rotted Redis password, a POSIX-only audit sink on an unsupported
    /// platform — `crates/darkmux-flow/src/lib.rs::build_default_sink`)
    /// share the SAME `"flow: "` prefix as its benign startup-success
    /// chatter, so the narrow chat filter above would otherwise make a
    /// genuinely degraded fleet stream invisible everywhere: every
    /// `/review` subprocess prints the warning, the chat filter eats it,
    /// and nothing anywhere says so. This proves the drop branch still
    /// hands every filtered line to its `record` callback — production
    /// wires that to `eprintln!` on this process's own stderr (asserted by
    /// inspection at the call site, not here — fd 2 is process-global and
    /// shared by every concurrently-running `cargo test` thread, so
    /// redirecting it mid-test would be flaky by construction; the
    /// callback-injection seam is what makes the DECISION testable without
    /// needing to capture real stderr).
    #[test]
    fn forwardable_chunk_text_still_records_a_filtered_degraded_mode_warning() {
        let flow_warning = "flow: Redis sink construction failed (connection refused); \
                             continuing without it. Other sinks intact.";
        let mut recorded = Vec::new();
        let result = forwardable_chunk_text_with_record(flow_warning, |line| recorded.push(line.to_string()));
        assert_eq!(result, None, "a degraded-mode flow warning still must not reach the chat verbatim");
        assert_eq!(
            recorded,
            vec![flow_warning.to_string()],
            "but it MUST still reach the record — never silently eaten"
        );

        // A line that reaches the chat must NOT also be pushed onto the
        // record path — the two are mutually exclusive per line, proving
        // this isn't a blanket "record everything" change in disguise.
        let mut recorded_real = Vec::new();
        let real_line = "some real progress line";
        let result = forwardable_chunk_text_with_record(real_line, |line| recorded_real.push(line.to_string()));
        assert_eq!(result, Some("`some real progress line`".to_string()));
        assert!(recorded_real.is_empty(), "a forwarded (non-filtered) line must not also be recorded");
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
    /// actual promise `kill_on_drop(true)` makes for `run_review`'s and
    /// `run_launch_command`'s real subprocess `Command`s — that the OS
    /// PROCESS itself dies, not just that the Rust future resolves — has
    /// rested on drop-topology reasoning alone until now.
    ///
    /// This proves that half directly: `tokio::spawn` a task that spawns
    /// a real, observably long-lived child (`sleep 30`, with
    /// `.kill_on_drop(true)` — the IDENTICAL flag `run_review`/
    /// `run_launch_command` set on their own `Command`s) and holds it
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
}
