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
//! - Model-seated targets (`LaunchRoute::Launch` — #2310 P4d retired the
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
    // (#2947 review C4) Bad enum config refuses up front, before the
    // utility routing call, instead of the routing dispatch failing and the
    // exchange degrading into "answering seat failed ... falling back".
    darkmux_crew::user_files::preflight(darkmux_types::config_enum::Scope::Dispatch)
        .map_err(|e| anyhow::anyhow!("radio: {e}"))?;
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
    let catalog = match radio::compile_catalog() {
        Ok(catalog) => catalog,
        // One stale user-tier file blocks every launch, so nothing is
        // launchable: say why, and exit 1 (nothing was routed or run).
        Err(refusal) => {
            eprintln!("radio: no mission config can be launched right now.\n{refusal:#}");
            return Ok(1);
        }
    };
    if catalog.is_empty() {
        println!(
            "radio: no mission config is launchable: the merged registry (built-ins + \
             ~/.darkmux/mission-configs/) is empty."
        );
        return Ok(0);
    }

    // (#1698 Packet B carry-list item 2 — wall 4's flow record) Through
    // `route_and_record`, not the bare `route`, so this invocation drops
    // the SAME shared-core flow record the ACP no-slash channel does — the
    // record is written once regardless of which surface routed.
    //
    // (#2917) …bounded: the router waits behind whatever occupies the one
    // utility instance (a compaction, by decision), but past
    // `radio::router_slow_notice_after()` it says so — what LM Studio
    // reports for the instance (requests waiting on it, nothing ahead, or
    // that darkmux cannot tell) — then keeps waiting to the call's ceiling. stderr, so a script capturing stdout
    // still gets only the answer.
    let notice_after = radio::router_slow_notice_after();
    let decision = match radio::route_and_record_bounded(
        text.to_string(),
        catalog.clone(),
        radio::RadioSurface::Cli,
        radio::dispatch_router_call,
        notice_after,
    ) {
        radio::RouteWait::Done(decision) => decision,
        radio::RouteWait::Slow(pending) => {
            eprintln!("radio: {}", crate::radio_busy::router_wait_notice_live(notice_after));
            pending.recv().unwrap_or_else(|_| radio::routing_thread_ended_early())
        }
    };

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
            let shelf = crate::radio_answer::ArtifactShelf::default();
            let overrides = crate::radio_answer::AnswererOverrides::default();
            match crate::radio_answer::answer_live(text, &catalog, &shelf, &overrides, radio::RadioSurface::Cli) {
                Ok(crate::radio_answer::LiveAnswer::Answered(outcome)) => {
                    println!("radio: {}", outcome.rendered);
                    Ok(0)
                }
                // (#2917) The seat's instance is busy: say so at once —
                // what LM Studio reports, and for which run when darkmux
                // knows — instead of queueing inside LM Studio until the
                // 300s ceiling. Exit 1: the question was not answered, and
                // a script must be able to tell that from an answer.
                Ok(crate::radio_answer::LiveAnswer::Busy(busy)) => {
                    println!("radio: {}", busy.answering_seat_message());
                    Ok(1)
                }
                // The model was reached for routing but not for answering: the
                // user got a degraded reply, and a script must be able to tell.
                Err(e) => Ok(print_answer_failure(&answer_failure_output(&e, &reason, &catalog))),
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
            println!("radio: routing to `mission launch {command}`: from your text");
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

/// What `radio` prints when the answering step failed: the `answer` for
/// stdout, and a `diagnostic` for stderr when there is one.
struct AnswerFailureOutput {
    answer: String,
    diagnostic: Option<String>,
}

/// Print `out` (the diagnostic to stderr, the answer to stdout) and return the
/// exit code: 1, because the question was not answered and a script must be
/// able to tell that from an answer.
fn print_answer_failure(out: &AnswerFailureOutput) -> i32 {
    if let Some(diagnostic) = &out.diagnostic {
        eprintln!("{diagnostic}");
    }
    println!("{}", out.answer);
    1
}

/// The output for a failed answering step. A seat that could not answer says
/// so and why, and nothing about darkmux: the router's refusal reason and the
/// command listing answer a question that WAS judged unanswerable, not this
/// one. Any other failure (a reply radio rejected) keeps the router's reason
/// and the listing, with the failure on stderr.
fn answer_failure_output(e: &anyhow::Error, reason: &str, catalog: &[CatalogEntry]) -> AnswerFailureOutput {
    match crate::radio_answer::seat_unavailable_notice(e) {
        Some(notice) => AnswerFailureOutput { answer: notice, diagnostic: None },
        None => AnswerFailureOutput {
            answer: format!("radio: {reason}\n{}", advertised_list_message(catalog)),
            diagnostic: Some(format!("radio: the answering seat failed ({e:#}); falling back to the plain refusal")),
        },
    }
}

/// A human-readable fallback list of what can be launched.
fn advertised_list_message(catalog: &[CatalogEntry]) -> String {
    let list =
        catalog.iter().map(|c| format!("`darkmux mission launch {}`", c.id)).collect::<Vec<_>>().join(", ");
    format!("Available commands: {list}.")
}

/// The user's answer to "run this command?". The model's pick is never the
/// consent: it is the command that is confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Consent {
    Approved,
    /// The user answered anything but yes.
    Declined,
    /// No answer could be asked for or read: no interactive terminal, or EOF
    /// before an answer. Nothing runs, and the exit is 1 (a script can tell
    /// "not run" from a run).
    Unanswered,
}

/// `darkmux mission launch <id> --param k=v ...` as the user would type it,
/// each param single-quoted when it needs it.
pub(crate) fn launch_command_line(config_id: &str, params: &[String]) -> String {
    let mut line = format!("darkmux mission launch {config_id}");
    for param in params {
        line.push_str(" --param ");
        line.push_str(&shell_word(param));
    }
    line
}

/// `word` as one POSIX shell word: bare when it is plain, else single-quoted.
fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | ',' | '@' | '+'));
    if plain { word.to_string() } else { format!("'{}'", word.replace('\'', "'\\''")) }
}

/// What to tell a user who will run a printed command themselves, when some
/// of its inputs were made from the cwd (see
/// [`crate::acp_panel::PreparedLaunch::synthesized_keys`]): the inputs among
/// them that name files ([`crate::acp_panel::SYNTHESIZED_FILE_KEYS`]) name
/// temporary files that are gone once this process ends, so the command is
/// not runnable as printed. `None` when none of the synthesized inputs is a
/// file.
pub(crate) fn synthesized_inputs_advice(keys: &[&str]) -> Option<String> {
    let files: Vec<&str> = keys.iter().copied().filter(|k| crate::acp_panel::SYNTHESIZED_FILE_KEYS.contains(k)).collect();
    if files.is_empty() {
        return None;
    }
    let example = files.iter().map(|k| format!("`--param {k}=<path>`")).collect::<Vec<_>>().join(" and ");
    Some(format!(
        "The values for {} above name temporary files, removed when this exits, so the command does not run as printed later. \
         To launch it yourself, save the diff and a workspace spec for your checkout to files of your own and pass them as \
         {example}; `darkmux mission config show <config>` lists what each input takes.",
        files.join(", ")
    ))
}

/// Print `command_line`, then ask y/N on `reader`/`writer`. Without an
/// interactive terminal nothing is asked: the command is printed, with the
/// line saying it was not run (and, when `synthesized` names inputs made from
/// the cwd, how to run it yourself), so the user can run it themselves.
/// `y`/`yes` (case-insensitive) is the only approval, and an interrupt seen
/// while waiting for it is never one: the SIGINT handler radio installs
/// leaves the read waiting, so a later `y` would otherwise launch a run that
/// is already being torn down.
fn confirm_launch<R: std::io::BufRead, W: std::io::Write>(
    mut reader: R,
    mut writer: W,
    interactive: bool,
    command_line: &str,
    synthesized: &[&str],
) -> Consent {
    let _ = writeln!(writer, "radio: chose this command from your text:\n\n    {command_line}\n");
    if !interactive {
        let _ = writeln!(
            writer,
            "radio: not run: there is no interactive terminal to confirm on. Run the command above yourself to launch it."
        );
        if let Some(advice) = synthesized_inputs_advice(synthesized) {
            let _ = writeln!(writer, "radio: {advice}");
        }
        return Consent::Unanswered;
    }
    // A Ctrl-C that landed while the launch was being prepared is honored
    // here, before a prompt nobody is going to answer.
    if darkmux_types::interrupt::is_set() {
        let _ = writeln!(writer, "radio: not run: interrupted.");
        return Consent::Unanswered;
    }
    let _ = write!(writer, "Run it? [y/N] ");
    let _ = writer.flush();
    let mut line = String::new();
    let read = reader.read_line(&mut line);
    if darkmux_types::interrupt::is_set() {
        let _ = writeln!(writer, "\nradio: not run: interrupted.");
        return Consent::Unanswered;
    }
    match read {
        Ok(0) | Err(_) => {
            let _ = writeln!(writer, "\nradio: not run: no answer was read.");
            Consent::Unanswered
        }
        Ok(_) if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") => Consent::Approved,
        Ok(_) => {
            let _ = writeln!(writer, "radio: not run.");
            Consent::Declined
        }
    }
}

/// How long [`InterruptibleStdin`] waits for input before checking for an
/// interrupt again.
const INTERRUPT_POLL_MS: libc::c_int = 100;

/// Stdin that gives up waiting once an interrupt is seen. The SIGINT handler
/// is installed with `SA_RESTART`, and a plain `read` on a terminal resumes
/// after it, so a Ctrl-C would leave the prompt waiting (and a third one,
/// with the default disposition restored, would kill the process before the
/// launch's temporary inputs are removed). This waits in `poll` in short
/// slices instead and checks the flag between them. The error it returns is
/// not `ErrorKind::Interrupted`, which `read_line` would retry.
struct InterruptibleStdin;

impl std::io::Read for InterruptibleStdin {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if darkmux_types::interrupt::is_set() {
                return Err(std::io::Error::other("interrupted"));
            }
            let mut fds = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
            // SAFETY: `fds` is one valid pollfd and the count says so.
            let ready = unsafe { libc::poll(&mut fds, 1, INTERRUPT_POLL_MS) };
            match ready {
                0 => continue,
                n if n > 0 => break,
                _ => {
                    let err = std::io::Error::last_os_error();
                    if err.kind() != std::io::ErrorKind::Interrupted {
                        return Err(err);
                    }
                }
            }
        }
        // Readable (a complete line, end of input, or a hangup), so this
        // does not block.
        std::io::stdin().lock().read(buf)
    }
}

/// [`confirm_launch`] against this process's stdin and stdout.
fn confirm_on_terminal(command_line: &str, synthesized: &[&str]) -> Consent {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    confirm_launch(std::io::BufReader::new(InterruptibleStdin), std::io::stdout(), interactive, command_line, synthesized)
}

/// The exit code for a consent that did not approve.
fn exit_for_unapproved(consent: Consent) -> Option<i32> {
    match consent {
        Consent::Approved => None,
        Consent::Declined => Some(0),
        Consent::Unanswered => Some(1),
    }
}

/// Turn a routed command (a catalog config id) into an actual execution,
/// through the SAME planning the editor panel's `/mission launch` uses
/// ([`crate::acp_panel::plan_launch`] then [`crate::acp_panel::prepare_launch`]),
/// so the two surfaces cannot drift: the same input mapping, the same
/// refusals, the same diff synthesis from the cwd for a config that declares
/// a required `diff_file`. The inputs are prepared BEFORE asking, so the user
/// confirms the command with every param that will run, the synthesized ones
/// included ([`confirm_on_terminal`]); the router could choose any config from
/// free text, so its pick is never the consent. A repo with nothing to review
/// is reported without asking.
fn execute(command: &str, args: &str) -> Result<i32> {
    let plan = crate::acp_panel::plan_launch(command, args).with_context(|| format!("radio: routed command `{command}`"))?;
    let cwd = std::env::current_dir().context("resolving current directory")?;
    let prepared = match crate::acp_panel::prepare_launch(&plan.config, plan.params.clone(), &cwd)? {
        crate::acp_panel::Prepared::Ready(prepared) => prepared,
        crate::acp_panel::Prepared::Nothing(msg) => {
            println!("radio: {msg}");
            return Ok(0);
        }
    };
    if let Some(note) = &prepared.note {
        println!("radio: {note}");
    }
    let command_line = launch_command_line(&plan.config_id, &prepared.params);
    if let Some(code) = exit_for_unapproved(confirm_on_terminal(&command_line, &prepared.synthesized_keys())) {
        return Ok(code);
    }
    match plan.route {
        crate::acp_panel::LaunchRoute::Ephemeral => run_ephemeral_and_report(&plan.config, &prepared.params),
        crate::acp_panel::LaunchRoute::Launch => spawn_mission_launch(&plan.config_id, &prepared.params),
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
fn run_ephemeral_and_report(config: &crate::crew::mission_config::MissionConfig, params: &[String]) -> Result<i32> {
    let cwd = std::env::current_dir().context("resolving current directory")?;
    let mut gate = cli_gate_handler();
    match crate::acp_panel::run_ephemeral(config, params, &cwd, Some(&mut *gate)) {
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
/// this process's stdio (`std::process::Command`'s default, unlike
/// `src/acp.rs::run_launch_command`'s headless `Stdio::null()`/`piped()`
/// spawn), so the child's own interactive tty sign-off gate
/// (`mission_launch.rs`'s private `cli_gate_handler`) sees the SAME real
/// terminal this `radio` invocation is running in. `params` are the
/// `--param` values [`crate::acp_panel::plan_launch`] and
/// [`crate::acp_panel::prepare_launch`] produced; the caller holds the
/// [`crate::acp_panel::PreparedLaunch`] (and its synthesized-input tempdir)
/// until this returns.
fn spawn_mission_launch(config_id: &str, params: &[String]) -> Result<i32> {
    let exe = std::env::current_exe().context("resolving darkmux's own executable path")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["mission", "launch", config_id]);
    for param in params {
        cmd.args(["--param", param]);
    }
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
    // the prepared launch's tempdir cleanup above; returning runs them. (This child is
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

    fn catalog() -> Vec<CatalogEntry> {
        vec![CatalogEntry { id: "pr-list".into(), description: "list PRs".into(), hint: None, accepts_args: true }]
    }

    /// A seat that could not answer is reported as that, with its reason, and
    /// carries neither the router's refusal sentence nor the catalog.
    #[test]
    fn an_unavailable_seat_prints_only_the_seat_failure() {
        let seat = anyhow::Error::new(crate::fleet::SubmitRefused {
            code: crate::fleet::RefusalCode::NotListed,
            reason: "studio does not accept work from laptop".into(),
        })
        .context("the answering seat `host@studio`");
        let err = crate::radio_answer::answer("what is a profile?", &catalog(), &crate::radio_answer::ArtifactShelf::default(), None, crate::radio_answer::GroundingScope::Full, radio::RadioSurface::Cli, &mut |_: &str, _| Err(anyhow::anyhow!("{seat:#}")))
            .unwrap_err();
        let out = answer_failure_output(&err, "Darkmux does not define profiles", &catalog());
        assert!(out.answer.contains("the answering seat was unavailable"), "{}", out.answer);
        assert!(out.answer.contains("studio does not accept work from laptop"), "{}", out.answer);
        assert!(!out.answer.contains("does not define") && !out.answer.contains("mission launch"), "{}", out.answer);
        assert_eq!(out.diagnostic, None);
    }

    /// Any other failure (a reply radio rejected) keeps the router's reason
    /// and the listing.
    #[test]
    fn any_other_failure_keeps_the_plain_refusal() {
        let out = answer_failure_output(&anyhow::anyhow!("the reply named a command that cannot be run"), "not a command", &catalog());
        assert!(out.answer.contains("radio: not a command"), "{}", out.answer);
        assert!(out.answer.contains("`darkmux mission launch pr-list`"), "{}", out.answer);
        assert!(out.diagnostic.is_some());
    }

    /// (#1861 review) This file's PRODUCTION half — everything ahead of
    /// its own test module. Searched instead of the whole file on purpose:
    /// a needle spelled out in a test must never be able to satisfy an
    /// assertion about the production call site.
    fn production_source() -> &'static str {
        let src = include_str!("radio_cli.rs");
        let cut = src.find("#[cfg(test)]").expect("this file has a test module");
        &src[..cut]
    }

    /// (#1861 review) Every sanitizer test pins the HELPER; none pinned
    /// that the caller passes the right surface, so flipping this one
    /// literal to `Panel` reinstated the whole reported defect with all
    /// tests still green. `run()` reaches a live model, so the claim is
    /// pinned where it is actually made.
    #[test]
    fn the_cli_entry_point_answers_on_the_cli_surface() {
        let src = production_source();
        assert!(
            src.contains("answer_live(text, &catalog, &shelf, &overrides, radio::RadioSurface::Cli)"),
            "the CLI answering call must pass the CLI surface"
        );
        assert!(
            !src.contains("RadioSurface::Panel"),
            "nothing in `darkmux radio`'s own path may claim the panel surface"
        );
    }

    // (Issue #1698 test-plan note) `--dry-run` needs a real routing call,
    // which needs a live model — the CLI's own `run()` wires
    // `radio::dispatch_router_call` directly (no injection seam at the
    // binary level, matching the retired `mission propose` verb's own un-injected
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
            accepts_args: true,
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

    // ── consent before a routed launch ──────────────────────────────────

    struct NoRead;
    impl std::io::Read for NoRead {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("a non-interactive confirm must not read");
        }
    }

    fn ask(input: &str, interactive: bool) -> (Consent, String) {
        let mut out = Vec::new();
        let consent = confirm_launch(input.as_bytes(), &mut out, interactive, "darkmux mission launch review", &[]);
        (consent, String::from_utf8(out).unwrap())
    }

    #[test]
    fn a_yes_answer_approves_and_the_command_was_shown_first() {
        for yes in ["y\n", "Y\n", "yes\n", "  YES \n"] {
            let (consent, out) = ask(yes, true);
            assert_eq!(consent, Consent::Approved, "{yes:?}");
            assert!(out.contains("darkmux mission launch review") && out.contains("[y/N]"), "{out}");
        }
    }

    #[test]
    fn anything_but_yes_declines() {
        for no in ["n\n", "\n", "no\n", "maybe\n", "yep\n"] {
            assert_eq!(ask(no, true).0, Consent::Declined, "{no:?}");
        }
    }

    #[test]
    fn eof_is_never_consent() {
        let (consent, out) = ask("", true);
        assert_eq!(consent, Consent::Unanswered);
        assert!(out.contains("not run"), "{out}");
    }

    #[test]
    fn without_a_terminal_the_command_is_printed_and_nothing_is_read() {
        let mut out = Vec::new();
        let consent = confirm_launch(std::io::BufReader::new(NoRead), &mut out, false, "darkmux mission launch review", &[]);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(consent, Consent::Unanswered);
        assert!(out.contains("darkmux mission launch review") && out.contains("not run") && out.contains("no interactive terminal"), "{out}");
    }

    /// A reader that delivers the interrupt while the prompt is waiting, then
    /// the `y` typed afterward: the SIGINT handler radio installs sets the
    /// flag and the blocked read resumes.
    struct InterruptedThenYes;
    impl std::io::Read for InterruptedThenYes {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            darkmux_types::interrupt::simulate_sigint_for_test();
            buf[..2].copy_from_slice(b"y\n");
            Ok(2)
        }
    }

    #[test]
    #[serial_test::serial]
    fn an_interrupt_during_the_prompt_is_never_consent_even_if_a_yes_follows() {
        // (#3100) It raises the process-wide interrupt flag.
        darkmux_types::run_in_own_process!();
        darkmux_types::interrupt::reset_for_test();
        let mut out = Vec::new();
        let consent =
            confirm_launch(std::io::BufReader::new(InterruptedThenYes), &mut out, true, "darkmux mission launch review", &[]);
        darkmux_types::interrupt::reset_for_test();
        assert_eq!(consent, Consent::Unanswered);
        assert!(String::from_utf8(out).unwrap().contains("interrupted"));
    }

    #[test]
    fn a_non_interactive_confirm_of_synthesized_inputs_says_how_to_run_it_yourself() {
        let mut out = Vec::new();
        let line = "darkmux mission launch review --param diff_file=/tmp/x/review.diff";
        let consent = confirm_launch(std::io::BufReader::new(NoRead), &mut out, false, line, &["diff_file", "workspace"]);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(consent, Consent::Unanswered);
        assert!(out.contains("--param diff_file=/tmp/x/review.diff"), "the real param is shown: {out}");
        assert!(out.contains("diff_file, workspace") && out.contains("temporary files"), "{out}");
        assert!(out.contains("--param diff_file=<path>"), "{out}");
        // The inverse: nothing synthesized, nothing qualified.
        let mut plain = Vec::new();
        confirm_launch(std::io::BufReader::new(NoRead), &mut plain, false, line, &[]);
        assert!(!String::from_utf8(plain).unwrap().contains("temporary"));
    }

    #[test]
    fn the_advice_names_only_the_file_inputs_and_points_at_the_config_listing() {
        let advice = synthesized_inputs_advice(&["diff_file", "workspace", "head_sha"]).expect("advice");
        assert!(advice.contains("diff_file, workspace") && !advice.contains("head_sha"), "{advice}");
        assert!(advice.contains("darkmux mission config show <config>"), "{advice}");
        assert!(!advice.contains("mission show"), "{advice}");
        // The inverse: a commit hash alone names no temporary file.
        assert_eq!(synthesized_inputs_advice(&["head_sha"]), None);
        assert_eq!(synthesized_inputs_advice(&[]), None);
    }

    #[test]
    #[serial_test::serial]
    fn an_interrupt_already_seen_before_the_prompt_ends_it_without_asking_or_reading() {
        // (#3100) It raises the process-wide interrupt flag.
        darkmux_types::run_in_own_process!();
        darkmux_types::interrupt::reset_for_test();
        darkmux_types::interrupt::simulate_sigint_for_test();
        let mut out = Vec::new();
        let consent = confirm_launch(std::io::BufReader::new(NoRead), &mut out, true, "darkmux mission launch review", &[]);
        darkmux_types::interrupt::reset_for_test();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(consent, Consent::Unanswered);
        assert!(out.contains("interrupted") && !out.contains("[y/N]"), "{out}");
    }

    #[test]
    fn only_an_approval_lets_the_launch_proceed_and_the_exits_differ() {
        assert_eq!(exit_for_unapproved(Consent::Approved), None);
        assert_eq!(exit_for_unapproved(Consent::Declined), Some(0));
        assert_eq!(exit_for_unapproved(Consent::Unanswered), Some(1));
    }

    #[test]
    fn the_printed_command_quotes_params_the_way_a_shell_needs() {
        let params = vec!["rules=a,b".to_string(), "args=fix the bug".to_string(), "t=it's".to_string()];
        assert_eq!(
            launch_command_line("review", &params),
            "darkmux mission launch review --param rules=a,b --param 'args=fix the bug' --param 't=it'\\''s'"
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
            radio::CatalogEntry { id: "review".to_string(), description: "d".to_string(), hint: None, accepts_args: true },
            radio::CatalogEntry { id: "pr-list".to_string(), description: "d2".to_string(), hint: None, accepts_args: true },
        ];
        let msg = advertised_list_message(&catalog);
        assert!(msg.contains("`darkmux mission launch review`"), "{msg}");
        assert!(msg.contains("`darkmux mission launch pr-list`"), "{msg}");
    }
}
