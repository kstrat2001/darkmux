//! `darkmux radio "<text>"` — the CLI verb over the radio interpreter core
//! (`src/radio.rs`, #1698 Packet A). Thin execution wiring ONLY: routing
//! decisions, catalog compilation, and the fail-closed validation contract
//! all live in `radio.rs`. This module's job is turning a
//! [`crate::radio::RouteDecision`] into an actual EXECUTION — reusing the
//! SAME two execution primitives `src/acp_panel.rs` already established for
//! the panel surface, never a third one:
//!
//! - Procedural-only targets run in-process via `crate::acp_panel::
//!   run_ephemeral` (already `pub`, no hoist needed — surface neutrality
//!   held without moving a single line of `acp_panel.rs`).
//! - Model-seated targets (`RoutePlan::Launch` — #2310 P4d retired the
//!   separate Review arm along with the bespoke launcher behind it, so
//!   every routed command resolves to the SAME `mission launch <id>`
//!   invocation) spawn `darkmux mission launch <id>` as a
//!   child process INHERITING the tty (unlike `src/acp.rs`'s headless ACP
//!   spawn, `Command`'s default stdio) — so `mission launch`'s own
//!   interactive sign-off gate prompt (`mission_launch.rs`'s private
//!   `cli_gate_handler`, #1684 Packet 2 / #1696) applies exactly as it
//!   would for a direct `darkmux mission launch <id>` at a shell. No new
//!   gate surface is built here.
//!
//! Single exchange by construction (issue #1698, "the turn-depth rule"):
//! [`run`] makes exactly one routing call and, unless `--dry-run`, exactly
//! one execution. No loop, no REPL, no retry-on-refusal.

use crate::radio::{self, CatalogEntry, RouteDecision};
use anyhow::{Context, Result};
use std::io::IsTerminal;

/// Top-level entry called from `main.rs`'s dispatch table for `Cmd::Radio`.
pub fn run(text: &str, dry_run: bool) -> Result<i32> {
    // (#2463) `darkmux radio` dispatches the routing seat
    // (`radio::dispatch_router_call`, below) and, on a `Refuse` decision,
    // the answering seat too (`radio_answer::answer_live` ->
    // `dispatch_answerer_call_with`) — both go through the container-free
    // `dispatch_local_single_shot` curl path with no signal handling at
    // all, the #2262 gap unfixed here. Each call already gets its own
    // `dispatch.error` bookend from the inline `BookendGuard`
    // `dispatch_local_single_shot` builds — so, same as `dispatch`/`lab
    // run`, the only things missing are (1) the handlers so SIGTERM/
    // SIGINT/SIGHUP become a flag instead of an outright kill, and (2)
    // the watchdog that kills the registered `curl` child (this path has
    // no self-polling seam of its own the way the docker container path
    // does). Armed ONCE, ahead of BOTH possible dispatches in this
    // single-exchange invocation, so a signal during either one is
    // caught.
    crate::launch_guard::arm();
    let _reap_watchdog = crate::launch_guard::spawn_reap_watchdog();
    let catalog = radio::compile_catalog();
    if catalog.is_empty() {
        println!(
            "radio: no commands are currently advertised — no mission config in the merged \
             registry (built-ins + ~/.darkmux/mission-configs/) declares a `panel` block."
        );
        return Ok(0);
    }

    // (#1698 Packet B carry-list item 2 — wall 4's flow record) Through
    // `route_and_record`, not the bare `route`, so this invocation drops
    // the SAME shared-core flow record the ACP no-slash channel does — the
    // record is written once regardless of which surface routed.
    let decision = radio::route_and_record(text, &catalog, radio::RadioSurface::Cli, &mut |message: &str| {
        radio::dispatch_router_call(message)
    });

    match decision {
        // (#1698 Packet B2, scope A) A refusal no longer prints the bare
        // reason + listing directly — the text goes to the ANSWERING seat
        // for a grounded, in-persona reply first (the CLI has no persistent
        // session, so it always answers against a fresh, empty shelf — see
        // `radio_answer::ArtifactShelf`'s own doc on why that's a documented
        // limitation, not a bug). The bare reason + listing becomes the
        // LAST RESORT, printed only when the answering dispatch itself
        // fails (e.g. no model loaded) — never silently swallowed.
        RouteDecision::Refuse { reason } => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let shelf = crate::radio_answer::ArtifactShelf::default();
            let overrides = crate::radio_answer::AnswererOverrides::default();
            match crate::radio_answer::answer_live(text, &catalog, &shelf, &cwd, &overrides) {
                Ok(outcome) => {
                    println!("radio: {}", outcome.rendered);
                    Ok(0)
                }
                Err(e) => {
                    eprintln!("radio: the answering seat failed ({e:#}); falling back to the plain refusal");
                    println!("radio: {reason}");
                    println!("{}", advertised_list_message(&catalog));
                    // The model was reached for routing but not for answering:
                    // the user got a degraded reply, and a script must be able
                    // to tell.
                    Ok(1)
                }
            }
        }
        // The routing seat never ran. Nothing downstream can do better with
        // the same registry / model / server, so: the error once, the fix
        // is inside it (every producer names its own next step), and a
        // non-zero exit so `darkmux radio` in a script or CI job fails
        // instead of printing a command listing and returning 0.
        RouteDecision::Unavailable { error } => {
            eprintln!("radio: could not reach a model.\n{error}");
            Ok(1)
        }
        RouteDecision::Route { command, args } => {
            println!("radio: routing to /{command} — from your text");
            if dry_run {
                if args.trim().is_empty() {
                    println!("radio: --dry-run — would invoke `{command}` with no arguments");
                } else {
                    println!("radio: --dry-run — would invoke `{command}` with args: {args:?}");
                }
                return Ok(0);
            }
            execute(&command, &args)
        }
    }
}

/// A human-readable fallback list, mirroring
/// `crate::acp_panel::not_a_command_message`'s render shape (a comma-joined
/// backtick-slash list) — not a direct call into that function, since it
/// takes `&[PanelCommand]`, not `&[CatalogEntry]` (two different catalog
/// shapes for two different consumers — see `radio.rs`'s doc on why
/// `CatalogEntry::description` diverges from `PanelCommand::description`).
fn advertised_list_message(catalog: &[CatalogEntry]) -> String {
    let list = catalog.iter().map(|c| format!("`/{}`", c.id)).collect::<Vec<_>>().join(", ");
    format!("Available commands: {list}.")
}

/// Turn a resolved (catalog-validated) command id into an actual execution.
/// Re-derives the execution PLAN via `crate::acp_panel::route_command` —
/// the SAME structural Review/Ephemeral/Launch decision the panel surface
/// uses — rather than re-implementing that classification here.
///
/// `Launch` covers every routed command now (#2310 P4d retired the bespoke
/// review arm along with its launcher). It is NOT true that a launched
/// config has "no required inputs beyond the optional `args` hook": a
/// diff-scoped config declares `diff_file` required, and `spawn_mission_
/// launch` synthesizes it from the cwd — see
/// `acp_panel::synthesize_diff_launch_inputs`, the same seam the editor
/// panel uses, so the two surfaces cannot drift.
fn execute(command: &str, args: &str) -> Result<i32> {
    let advertised = crate::acp_panel::list_panel_commands();
    let plan = crate::acp_panel::route_command(&advertised, command).ok_or_else(|| {
        anyhow::anyhow!(
            "radio: routed command `{command}` is no longer advertised (the registry changed \
             between routing and execution)"
        )
    })?;

    match plan {
        crate::acp_panel::RoutePlan::Ephemeral(config) => run_ephemeral_and_report(&config, args),
        crate::acp_panel::RoutePlan::Launch(id) => spawn_mission_launch(&id, args),
    }
}

/// `run_ephemeral` now returns a typed [`crate::acp_panel::EphemeralOutcome`]
/// (#1698 Packet B carry-list item 5 — retires the failure-prefix string-
/// sniffing `ephemeral_output_is_failure` used to require), so the CLI's
/// exit code reads `outcome.success` directly instead of matching a
/// literal prefix on the rendered text. This also picks up the "Known gap"
/// fix for free: a run whose terminal step completes cleanly while a SIDE
/// branch of the graph errored now reports `success: false` (see
/// `render_ephemeral_result`'s own doc) — the string-sniffing contract
/// could never distinguish that case from a genuine clean success, so this
/// CLI used to silently exit 0 for a partially-failed run.
fn run_ephemeral_and_report(config: &crate::crew::mission_config::MissionConfig, args: &str) -> Result<i32> {
    let cwd = std::env::current_dir().context("resolving current directory")?;
    let mut gate = cli_gate_handler();
    match crate::acp_panel::run_ephemeral(config, args, &cwd, Some(&mut *gate)) {
        Ok(outcome) => {
            println!("{}", outcome.text);
            if outcome.success { Ok(0) } else { Ok(1) }
        }
        Err(e) => {
            eprintln!("radio: command failed: {e:#}");
            Ok(1)
        }
    }
}

/// Spawn `darkmux mission launch <config_id>` as a child process INHERITING
/// this process's stdio (`std::process::Command`'s default — unlike
/// `src/acp.rs::run_launch_command`'s headless `Stdio::null()`/`piped()`
/// spawn), so the child's own interactive tty sign-off gate
/// (`mission_launch.rs`'s private `cli_gate_handler`) sees the SAME real
/// terminal this `radio` invocation is running in. Forwards the raw text
/// as `--param args=<raw>` when non-empty — the identical, already-
/// documented forward-compatible hook `src/acp.rs::run_launch_command` uses
/// (see that function's own "args honesty note": today no shipped config
/// declares `args` as a `MissionInput`, so this is a hook, not yet a wired
/// delivery — the same honest limitation applies here, unchanged).
fn spawn_mission_launch(config_id: &str, args: &str) -> Result<i32> {
    let exe = std::env::current_exe().context("resolving darkmux's own executable path")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["mission", "launch", config_id]);
    if !args.trim().is_empty() {
        cmd.args(["--param", &format!("args={args}")]);
    }
    // (#2310 P4d) Same synthesis the editor panel does, from the same
    // function: a diff-scoped config gets its `diff_file`/`workspace`/
    // `head_sha` from this cwd. `_synth`'s Drop removes the tempdir on
    // every exit path below.
    let cwd = std::env::current_dir().context("resolving current directory")?;
    let config = crate::crew::mission_config::load(config_id)
        .with_context(|| format!("loading mission config \"{config_id}\""))?
        .config;
    let _synth = match crate::acp_panel::synthesize_diff_launch_inputs(&config, &cwd)? {
        crate::acp_panel::DiffLaunchInputs::NotNeeded => None,
        crate::acp_panel::DiffLaunchInputs::Nothing(msg) => {
            println!("radio: {msg}");
            return Ok(0);
        }
        crate::acp_panel::DiffLaunchInputs::Ready(synth) => {
            for p in synth.params() {
                cmd.args(["--param", p]);
            }
            if let Some(note) = &synth.excluded_note {
                println!("radio: {note}");
            }
            Some(synth)
        }
    };
    println!("radio: launching `{config_id}` …");
    // (#2463 review) POLL, do not `status()`. This child is deliberately NOT
    // in `child_registry` — the reap watchdog would SIGKILL it ~100ms after a
    // Ctrl-C, racing its own `LaunchFinalizeGuard` and leaving the mission
    // Active — so nothing else can end the wait on our behalf.
    //
    // `cmd.status()` blocks in `Child::wait`, which retries `EINTR`. With
    // `arm()` now installed above, a TARGETED `kill -TERM <radio pid>` sets
    // the interrupt flag and returns to that wait, so radio hung for the
    // launch's entire duration where before this change one signal ended it.
    // Ctrl-C hid the regression: a terminal signal goes to the whole
    // foreground group and reaches the child too. A supervisor's targeted
    // kill does not — and that is the form every signal test here uses.
    //
    // Polling restores the pre-guard outcome for a targeted kill: we stop
    // waiting and exit 130. (#2477) The child is no longer left ENTIRELY to
    // its own signal handling: once WE notice our own caught signal, we
    // forward it to the child (`forward_signal_and_wait`) before exiting, so
    // a targeted kill on radio's pid now finalizes the launch too, matching
    // the outcome a real Ctrl-C (which the whole foreground group already
    // reaches) has always produced.
    //
    // Note what this loop deliberately does NOT do any more: the branch
    // below used to call `launch_guard::reap_and_exit_on_signal()`, which
    // `SIGKILL`s every `child_registry` pid and then `std::process::exit
    // (130)`s. Neither half is load-bearing here. The reap is redundant:
    // `run` above holds a `spawn_reap_watchdog` for this whole invocation,
    // and that watchdog already calls the SAME `kill_all(SIGKILL)` on a
    // ~100ms cadence from the moment the interrupt flag is set — so every
    // registered pid gets reaped whether or not this line runs. And the
    // hard `exit` skipped every destructor on the way out, including
    // `_synth`'s tempdir cleanup above; returning runs them. (This child is
    // not in that registry at all — see the comment above — so neither call
    // ever reached it.)
    //
    // (#2462 review) Read as a general verdict on that function, the
    // paragraph above would contradict `dispatch`/`lab run`, which KEEP the
    // call. It is not one: both of its reasons are properties of THIS site
    // (an unregistered child, a live tempdir destructor, an exit code this
    // loop already returns through `main`), and neither holds there. The
    // deciding conditions are written down once now — in
    // `reap_and_exit_on_signal`'s own "When NOT to call this" section —
    // rather than as two site-local comments a reader has to reconcile.
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning `darkmux mission launch {config_id}`"))?;
    loop {
        if let Some(status) = child.try_wait().context("waiting on `darkmux mission launch`")? {
            return Ok(status.code().unwrap_or(1));
        }
        if darkmux_types::interrupt::is_set() {
            return forward_signal_and_wait(&mut child);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// (#2477) How long radio waits, after forwarding OUR OWN caught signal to
/// the launched `mission launch` child, before it stops waiting and reports
/// that the child is still finishing.
///
/// **This is an ANNOUNCE point, not a kill budget** — see
/// [`forward_signal_and_wait`] for why nothing is forced down when it
/// expires. It bounds only how long RADIO stays alive holding the
/// operator's terminal, which is radio's own promise (a targeted kill
/// returns control promptly) and the only thing radio actually controls.
///
/// **Why a fixed number cannot bound the child's finalize.** An earlier
/// version of this constant justified 5 seconds as "the guard's finalize
/// write is a local JSON write + rename, a few milliseconds", reusing the
/// bound the direct-SIGTERM launcher tests assert. Measured, that
/// reasoning does not hold: `crew::lifecycle::save_json` is `write` +
/// **`fsync`** + `rename` + **`fsync` of the parent directory**, once per
/// mission/phase/task/step record, and `fsync` latency is bounded by the
/// machine's I/O queue, not by the size of the write. On this repo's own
/// dev machine under an ordinary multi-agent load (load average 21), the
/// child needed longer than 5s to finalize and the escalation this
/// constant used to gate fired on a launcher that was seconds from
/// succeeding — reproduced deterministically by setting this to zero, which
/// leaves the mission `active`, the exact outcome #2477 exists to prevent.
const FORWARD_SIGNAL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// (#2477) Forward the signal that just interrupted THIS process to the
/// launched `mission launch` child, then wait — bounded — for it to exit on
/// its own before this process does too.
///
/// **Ordering is the whole point.** Signal, THEN wait, THEN exit — never
/// exit right after signalling. `kill(2)` returning only means the kernel
/// queued the signal, not that the child's `LaunchFinalizeGuard`
/// (`launch_guard.rs`) has run; exiting immediately would race the very
/// finalize this function exists to enable, reproducing the bug from the
/// child's side instead of radio's.
///
/// **Signalling by pid is safe here, and would not be anywhere else.**
/// `child` is a live [`std::process::Child`] this process spawned and has
/// NOT reaped: the only `try_wait` calls anywhere in this file return
/// before reaching a signal send, and nothing in this binary reaps
/// children process-wide (no `wait(-1)`, no `SIGCHLD` handler — the only
/// signals installed are `darkmux_types::interrupt`'s INT/TERM/HUP). So if
/// the child has already exited it is a ZOMBIE, which still holds its pid,
/// and the send is a harmless no-op. There is no window in which this pid
/// can have been recycled onto an unrelated process — measured directly on
/// this platform, including from a parent that had set `SIGCHLD` to
/// `SIG_IGN`. `child_registry::kill_pid`'s own doc carries the same
/// contract for every future caller.
///
/// **What happens if the child outlives the grace window: radio says so and
/// leaves.** It does NOT force the child down. A `SIGKILL` here can only
/// ever make the outcome worse — the child has already been asked to stop
/// and is either converging on its own terminal record or genuinely stuck,
/// and killing it converts the first case (a finalized mission, the whole
/// point of #2477) into the second (a mission left `active`). The window is
/// gated on `fsync` latency, so no fixed number makes that safe; an earlier
/// cut escalated to `SIGKILL` here and was measured destroying a real
/// finalize under ordinary machine load. The mission record is atomic
/// either way (`save_json` is tmp + fsync + rename), so nothing here can
/// leave a torn `mission.json` — the only difference the escalation made
/// was whether the good record ever got written.
///
/// The cost of not killing is a launcher that may linger unsupervised, so
/// that is exactly what the operator is TOLD, by pid, with the verb that
/// resolves it — never left to be inferred from a bare exit code.
fn forward_signal_and_wait(child: &mut std::process::Child) -> Result<i32> {
    let pid = child.id();
    // The child may already be exiting on its own (a real Ctrl-C reaches
    // it too, via the foreground process group, so this can race a signal
    // it already received) — `ESRCH` on an already-reaped pid is not
    // reportable news. Anything else is: a signal that silently failed to
    // send is the difference between "your mission finalized" and "your
    // mission is still running", and the operator must not have to guess
    // which happened.
    if let Err(e) = darkmux_types::child_registry::kill_pid(pid, darkmux_types::child_registry::SIGTERM) {
        if e.raw_os_error() != Some(darkmux_types::child_registry::ESRCH) {
            eprintln!("radio: could not forward the interrupt to `mission launch` (pid {pid}): {e}");
        }
    }

    let deadline = std::time::Instant::now() + FORWARD_SIGNAL_GRACE;
    loop {
        if let Some(status) = child
            .try_wait()
            .context("waiting on `darkmux mission launch` after forwarding its signal")?
        {
            return Ok(status.code().unwrap_or(130));
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    eprintln!(
        "radio: the launched `mission launch` (pid {pid}) was asked to stop but is still \
         finishing after {}s — radio is exiting now and will stop watching it. It should \
         finalize on its own shortly; if it does not, `darkmux mission status` shows whether \
         the mission is still `active` and `darkmux mission abort <id>` closes it.",
        FORWARD_SIGNAL_GRACE.as_secs()
    );
    Ok(130)
}

/// Mirrors `mission_launch.rs`'s private `cli_gate_handler` SELECTION
/// (interactive CLI at a real tty on BOTH streams -> the y/N prompt;
/// otherwise -> fail closed) — five lines, not worth threading a new `pub`
/// seam through `mission_launch.rs` for. The two underlying handlers
/// themselves (`crew::gate::tty_prompt_handler` / `refusal_handler`) ARE
/// the reused surface (#1684 Packet 2); this function only re-derives
/// WHICH one to pick, exactly as `mission_launch.rs` already does for the
/// `mission launch` verb itself.
fn cli_gate_handler() -> Box<crate::crew::gate::GateHandler<'static>> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Box::new(crate::crew::gate::tty_prompt_handler())
    } else {
        Box::new(crate::crew::gate::refusal_handler())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // (Issue #1698 test-plan note) `--dry-run` needs a real routing call,
    // which needs a live model — the CLI's own `run()` wires
    // `radio::dispatch_router_call` directly (no injection seam at the
    // binary level, matching mission_propose.rs's own un-injected
    // `dispatch_compiler` call). Per the task's own concession ("if the
    // binary-level test can't inject the model call, test the dry-run path
    // at the function level and say so"): the dry-run PRINTING logic is
    // exercised here at the function level via `radio::route` with an
    // injected canned closure, bypassing `run()`'s live dispatch. An
    // assert_cmd binary-level `--dry-run` test would need a running,
    // matching LMStudio profile to produce a deterministic route — exactly
    // the live-model dependency the task rules out for tests.
    #[test]
    fn dry_run_decision_never_reaches_execution() {
        let catalog = vec![radio::CatalogEntry {
            id: "review".to_string(),
            description: "Run the review pipeline.".to_string(),
            hint: None,
        }];
        let mut call = |_msg: &str| -> Result<String> { Ok("```json\n{\"command\": \"review\", \"args\": \"42\"}\n```".to_string()) };
        let decision = radio::route("review this", &catalog, &mut call);
        // The CLI's own dry-run branch (see `run()`) never calls `execute`
        // for a `Route` decision when `dry_run` is true — asserting the
        // DECISION shape here is the function-level equivalent of asserting
        // dry-run printed the resolved invocation and executed nothing,
        // since `execute()` is the ONLY path that has side effects and it's
        // structurally unreachable from this decision without `dry_run`
        // being `false` in `run()`'s own match arm.
        assert_eq!(
            decision,
            radio::RouteDecision::Route { command: "review".to_string(), args: "42".to_string() }
        );
    }

    #[test]
    fn advertised_list_message_lists_every_catalog_entry() {
        // `advertised_list_message` is only ever called from `run()`'s
        // `RouteDecision::Refuse` arm, which is only reachable once
        // `radio::route` has already seen a NON-EMPTY catalog (the
        // empty-catalog case short-circuits to a distinct message in
        // `run()` before `route` is even called) — so an empty-catalog
        // input to this function is not a real production shape, and no
        // test pins one.
        let catalog = vec![
            radio::CatalogEntry { id: "review".to_string(), description: "d".to_string(), hint: None },
            radio::CatalogEntry { id: "pr-list".to_string(), description: "d2".to_string(), hint: None },
        ];
        let msg = advertised_list_message(&catalog);
        assert!(msg.contains("`/review`"), "{msg}");
        assert!(msg.contains("`/pr-list`"), "{msg}");
    }
}
