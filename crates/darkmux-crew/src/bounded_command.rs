//! Bounded execution of ONE operator-supplied shell command a step runs
//! (#2310 / #2361, swarm finding S4-4 — proven live).
//!
//! **The failure this exists to close.** `mods.gate` and
//! `procedural.shell` each ran their command as a plain
//! `Command::output()`: no deadline, and no child pid registered with
//! `darkmux_types::child_registry`. Three consequences, all observed on a
//! live run: a `test_command` that never returns pins the mission `Active`
//! forever; a caught SIGTERM/SIGINT does nothing until the command returns
//! on its own, because the launcher's guard reaps only REGISTERED pids and
//! there was none; and a `SIGKILL` to darkmux leaves the `sh` (and
//! whatever it spawned) running with a mission stuck open behind it.
//!
//! **Mechanism**, deliberately the same shape as the other two bounded
//! child runners in this codebase — `darkmux_flow::run_security_bounded`
//! (the Keychain read) and `darkmux_profiles::gestalt_host::lms_host` (the
//! model load, `DARKMUX_MODEL_LOAD_TIMEOUT_SECONDS`): `spawn` +
//! `try_wait` polling (std only, no `wait-timeout` crate), pipes drained on
//! detached threads so a chatty command can never deadlock on a full pipe
//! buffer, and a hard kill at expiry.
//!
//! **Two things it does that neither sibling needed:**
//!
//! 1. **The child gets its OWN process group** (`setpgid(0, 0)` in
//!    `pre_exec`) and expiry kills the GROUP, not just the pid. An
//!    operator's `test_command` is arbitrary shell — `sh -c "make test"`
//!    routinely forks — and killing only the `sh` leaves the real work
//!    orphaned. Note this is the child's group, never darkmux's own:
//!    `child_registry`'s module doc records exactly why darkmux must not
//!    move ITSELF into a fresh group (Ctrl-C delivery breaks under a
//!    non-interactive wrapper), and none of that reasoning applies to a
//!    child we spawn and fully own.
//! 2. **The pid is registered with [`darkmux_types::child_registry`]**
//!    before the wait, so a launcher's `LaunchFinalizeGuard` reaps it on a
//!    caught signal — and the poll loop watches
//!    `darkmux_types::interrupt::is_set()` itself, so the command stops
//!    promptly instead of holding the step (and the mission's terminal
//!    record) open until the guard's own exit path runs.
//!
//! The bound comes from `runtime.step_command_timeout_seconds`
//! (`env(DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS) > config.json > 600`), read
//! through `darkmux_types::config_access` like every other knob. `0` means
//! UNBOUNDED (#2310 fix-loop E2) — see [`configured_timeout`].
//!
//! 3. **A finished command's leftovers are killed too.** If the output
//!    drains have not reached EOF within [`DRAIN_GRACE`] after the command
//!    itself exited, something it spawned is holding the pipe; that
//!    process group is killed before the runner returns, so a
//!    `test_command` that starts a daemon leaks nothing.

use std::process::Command;
use std::time::{Duration, Instant};

/// How often the deadline loop polls `try_wait`. Short enough that a
/// caught signal turns into a dead child within a human blink, long enough
/// that a multi-minute suite costs a negligible number of wakeups.
const POLL: Duration = Duration::from_millis(50);

/// How long a finished command's output drains are given to reach EOF
/// before the runner takes what it has and moves on. Only reachable when
/// something the command spawned still holds the pipe open after the
/// command itself exited; a normal command's drains are already at EOF.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Which pipe a drain thread is reading — the two have different types and
/// this keeps the one drain body shared between them.
enum DrainSource {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

fn snapshot(buf: &std::sync::Mutex<Vec<u8>>) -> Vec<u8> {
    buf.lock().map(|b| b.clone()).unwrap_or_default()
}

/// What a bounded run produced. Every arm is a fact a caller can record —
/// none of them is "we don't know".
#[derive(Debug)]
pub enum Bounded {
    /// The command ran to completion within the bound.
    Finished {
        success: bool,
        code: Option<i32>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    /// The bound expired and the child's whole process group was killed.
    /// `seconds` is the bound that was applied, so a caller can name it.
    TimedOut { seconds: u64 },
    /// A signal reached darkmux while the command was running; the child's
    /// process group was killed so the launcher's finalize path can run.
    Interrupted,
    /// The command could not even be spawned (no `sh`, a workdir that
    /// vanished). Distinct from every other arm: nothing ran, so nothing
    /// about the command itself was measured.
    SpawnFailed(std::io::Error),
}

/// (#2310 fix-loop E2) The value [`configured_timeout`] returns for a
/// configured `0`. Not a real instant: the deadline is only ever compared
/// against with `started.elapsed() >= timeout`, which `Duration::MAX` can
/// never satisfy, so the bound simply never fires. Never added to an
/// `Instant`, which is the one thing `Duration::MAX` cannot survive.
pub const UNBOUNDED: Duration = Duration::MAX;

/// The configured bound, as a `Duration`.
///
/// (#2310 fix-loop E2, from the loop-D review) **`0` means UNBOUNDED**, the
/// same reading every other darkmux zero-knob has (`redis.maxlen: 0` is
/// unbounded retention, an absent `runtime.max_turns` is uncapped). It used
/// to mean "kill instantly": `started.elapsed() >= Duration::ZERO` is true
/// on the first poll, so an operator who wrote `0` intending "no bound" got
/// every step command killed after about a millisecond, with a
/// `TimedOut{0}` naming a bound they thought they had disabled.
pub fn configured_timeout() -> Duration {
    match darkmux_types::config_access::step_command_timeout_seconds() {
        0 => UNBOUNDED,
        n => Duration::from_secs(n),
    }
}

/// Run `cmd` under `timeout`, in its own process group, registered with the
/// child registry. See the module doc for why each of those matters.
///
/// Takes an already-built `Command` (cwd, env and args are the caller's
/// business) for the same reason `run_security_bounded` does: the runner
/// owns the BOUND, not the command.
pub fn run_bounded(mut cmd: Command, timeout: Duration) -> Bounded {
    use std::io::Read;
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: `setpgid` is async-signal-safe and touches only the
        // freshly-forked child, which is the one thing `pre_exec` is
        // allowed to do. It moves the CHILD into its own group; darkmux's
        // own group is never touched (see the module doc).
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Bounded::SpawnFailed(e),
    };
    let pid = child.id();
    darkmux_types::child_registry::register(pid);
    // The drains write into shared buffers and SIGNAL when they hit EOF,
    // rather than being `join`ed for their return value. A `join` here is
    // an unbounded wait in disguise: the pipe's write end is inherited by
    // every process the command forks, so a `test_command` that leaves
    // anything running (a `&`-backgrounded step, a daemon it starts) holds
    // stdout open AFTER the command itself has exited, and `read_to_end`
    // then never returns — reintroducing exactly the unbounded wait this
    // module exists to remove. Proven by this module's own tests: the
    // first cut joined, and the grandchild test hung the whole suite.
    let out_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let err_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let (eof_tx, eof_rx) = std::sync::mpsc::channel::<()>();
    for (pipe, buf) in [
        (child.stdout.take().map(DrainSource::Out), out_buf.clone()),
        (child.stderr.take().map(DrainSource::Err), err_buf.clone()),
    ] {
        let tx = eof_tx.clone();
        std::thread::spawn(move || {
            if let Some(src) = pipe {
                let mut reader: Box<dyn std::io::Read + Send> = match src {
                    DrainSource::Out(p) => Box::new(p),
                    DrainSource::Err(p) => Box::new(p),
                };
                let mut chunk = [0u8; 8192];
                while let Ok(n) = reader.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut b) = buf.lock() {
                        b.extend_from_slice(&chunk[..n]);
                    }
                }
            }
            let _ = tx.send(());
        });
    }
    drop(eof_tx);
    let started = Instant::now();
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Wait for BOTH drains to reach EOF, but only for a grace
                // period — see the drain comment above for why this is
                // never an unconditional join.
                let grace_until = Instant::now() + DRAIN_GRACE;
                let mut eofs = 0;
                for _ in 0..2 {
                    let left = grace_until.saturating_duration_since(Instant::now());
                    if eof_rx.recv_timeout(left).is_err() {
                        break;
                    }
                    eofs += 1;
                }
                if eofs < 2 {
                    // (#2310 fix-loop E2, from the loop-D review) The
                    // command exited but SOMETHING it spawned still holds
                    // the pipe. Walking away here leaked that process — one
                    // per gated mod for a `test_command` that starts a
                    // daemon — and paid the full grace every time. The
                    // group is ours (the child was `setpgid`-ed into its
                    // own), so killing it reaches exactly what the command
                    // left behind and nothing else. Output already drained
                    // is kept: the snapshot below reads what the buffers
                    // hold, which is everything written before the kill.
                    //
                    // (F2) Signaling `-pid` is still safe even though
                    // `try_wait` just REAPED that pid: a process-group id
                    // stays reserved for as long as any process remains in
                    // the group, so the kernel cannot recycle this pgid onto
                    // an unrelated process while the very holder that is
                    // keeping the pipe open sits in it. The reaped leader is
                    // what makes the group's id available to us; the live
                    // holder is what keeps it from meaning anything else. If
                    // the group had in fact emptied between the two, the
                    // signal fails with ESRCH and reaches nothing — so the
                    // window is a no-op, not a mis-target.
                    kill_group(pid);
                }
                break Bounded::Finished {
                    success: status.success(),
                    code: status.code(),
                    stdout: snapshot(&out_buf),
                    stderr: snapshot(&err_buf),
                };
            }
            Ok(None) => {}
            // The child is unwaitable (already reaped by something else) —
            // treat it as finished-unknown rather than spinning forever.
            Err(e) => {
                break Bounded::SpawnFailed(e);
            }
        }
        if darkmux_types::interrupt::is_set() {
            kill_group(pid);
            let _ = child.wait();
            break Bounded::Interrupted;
        }
        if started.elapsed() >= timeout {
            kill_group(pid);
            let _ = child.wait();
            break Bounded::TimedOut { seconds: timeout.as_secs() };
        }
        std::thread::sleep(POLL);
    };
    darkmux_types::child_registry::deregister(pid);
    outcome
}

/// SIGKILL the child's whole process group, then the pid itself as a
/// backstop (a child that failed its own `setpgid` is still in darkmux's
/// group, where the negative-pid kill must NOT be sent — killing
/// `-getpgrp()` would take darkmux down with it, so the group kill is only
/// ever addressed to the child's own pid-as-pgid).
#[cfg(unix)]
fn kill_group(pid: u32) {
    let pid = pid as libc::pid_t;
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
        libc::kill(pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(command: &str) -> Command {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    }

    /// A sleep duration no other process on this machine is using — the
    /// test binary's own pid as a fractional part, so a leftover from an
    /// EARLIER run (or another worktree's suite) can never satisfy the
    /// `pgrep` these tests do. BSD/GNU `sleep` both take a decimal.
    fn marker(tag: u32) -> String {
        format!("861{tag}.{}", std::process::id())
    }

    /// `pgrep -f "sleep <marker>"`, retried for up to ~3s — a group kill is
    /// delivered synchronously but the kernel reaps asynchronously, and a
    /// loaded machine can lag. Returns the surviving pids (empty = nothing
    /// left).
    fn survivors(m: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let found = Command::new("pgrep")
                .args(["-f", &format!("sleep {m}")])
                .output()
                .expect("pgrep must be available");
            let out = String::from_utf8_lossy(&found.stdout).trim().to_string();
            if out.is_empty() || Instant::now() >= deadline {
                return out;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// (regression, found by this module's own suite) A command that
    /// leaves something running and exits must not hang the runner. The
    /// forked process inherits the stdout pipe's write end, so `read_to_end`
    /// on that pipe never returns — the first cut of this module `join`ed
    /// the drain threads and deadlocked the whole test binary that way.
    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn a_command_that_leaves_a_process_holding_stdout_still_returns() {
        let m = marker(5);
        let started = Instant::now();
        let out = run_bounded(sh(&format!("sleep {m} & printf ok")), Duration::from_secs(600));
        let elapsed = started.elapsed();
        // (#2310 fix-loop E2) The runner now kills the pipe-holding
        // group itself, so this is belt-and-braces rather than the
        // cleanup it used to be — see
        // `a_finished_command_that_leaves_a_process_holding_stdout_has_its_group_killed`.
        let _ = Command::new("pkill").args(["-f", &format!("sleep {m}")]).status();
        match out {
            Bounded::Finished { success, stdout, .. } => {
                assert!(success);
                assert_eq!(String::from_utf8_lossy(&stdout), "ok");
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(10),
            "a held-open pipe must not block the runner, took {elapsed:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn a_quick_command_finishes_with_its_output_and_code() {
        match run_bounded(sh("printf hi; exit 3"), Duration::from_secs(20)) {
            Bounded::Finished { success, code, stdout, .. } => {
                assert!(!success);
                assert_eq!(code, Some(3));
                assert_eq!(String::from_utf8_lossy(&stdout), "hi");
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    /// (#2361, S4-4) The bound is real: a 300-second sleep under a 2-second
    /// deadline returns in about the deadline, not in five minutes.
    #[test]
    #[serial_test::serial]
    fn a_command_past_the_deadline_is_killed_and_named() {
        let started = Instant::now();
        let out = run_bounded(sh("sleep 300"), Duration::from_secs(2));
        let elapsed = started.elapsed();
        assert!(
            matches!(out, Bounded::TimedOut { seconds: 2 }),
            "expected TimedOut{{2}}, got {out:?}"
        );
        assert!(
            elapsed < Duration::from_secs(6),
            "the deadline must return promptly, took {elapsed:?}"
        );
    }

    /// The kill reaches the whole GROUP, not just the `sh`: a command that
    /// forks leaves nothing behind. The marker is a unique sleep duration
    /// so `pgrep` can only match this test's own child.
    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn the_kill_reaches_a_forked_grandchild() {
        let m = marker(3);
        let out = run_bounded(sh(&format!("sleep {m} & wait")), Duration::from_secs(2));
        assert!(matches!(out, Bounded::TimedOut { .. }), "got {out:?}");
        let left = survivors(&m);
        let _ = Command::new("pkill").args(["-f", &format!("sleep {m}")]).status();
        assert!(left.is_empty(), "a forked grandchild survived the group kill: {left}");
    }

    /// (#2361, S4-4) A caught SIGTERM/SIGINT reaches the command: the run
    /// stops promptly, the child's whole group is killed (nothing
    /// orphaned), and the caller gets a fact it can record — which is what
    /// lets the launcher's `LaunchFinalizeGuard` write the mission's
    /// terminal record instead of the mission sitting Active until a
    /// `test_command` that may never return does.
    #[test]
    #[serial_test::serial] // the interrupt flag is process-wide
    #[cfg(unix)]
    fn a_caught_signal_kills_the_child_group_and_returns_interrupted() {
        let m = marker(4);
        darkmux_types::interrupt::reset_for_test();
        darkmux_types::interrupt::simulate_sigterm_for_test();
        let started = Instant::now();
        let out = run_bounded(sh(&format!("sleep {m} & wait")), Duration::from_secs(600));
        let elapsed = started.elapsed();
        darkmux_types::interrupt::reset_for_test();
        assert!(matches!(out, Bounded::Interrupted), "got {out:?}");
        assert!(elapsed < Duration::from_secs(5), "a signal must not wait out the bound, took {elapsed:?}");
        let left = survivors(&m);
        let _ = Command::new("pkill").args(["-f", &format!("sleep {m}")]).status();
        assert!(left.is_empty(), "a child survived the signal path: {left}");
    }

    /// The pid is REGISTERED while the command runs, which is the whole
    /// mechanism by which a launcher's `LaunchFinalizeGuard` reaps it on a
    /// caught signal. Proven from the outside, the way the guard does it:
    /// `child_registry::kill_all` — which knows only registered pids —
    /// must end a command that would otherwise run for minutes.
    #[test]
    #[serial_test::serial] // the child registry is process-wide
    fn the_running_child_is_registered_so_the_guards_kill_all_can_reap_it() {
        darkmux_types::child_registry::reset_for_test();
        let handle = std::thread::spawn(|| run_bounded(sh("sleep 240"), Duration::from_secs(600)));
        // Give the spawn + registration a beat, then reap the way the
        // launch guard does.
        std::thread::sleep(Duration::from_millis(400));
        let started = Instant::now();
        darkmux_types::child_registry::kill_all(darkmux_types::child_registry::SIGKILL);
        let out = handle.join().expect("the bounded run must not panic");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "kill_all must reach the registered child, took {:?}",
            started.elapsed()
        );
        assert!(matches!(out, Bounded::Finished { success: false, .. }), "got {out:?}");
        darkmux_types::child_registry::reset_for_test();
    }

    /// (#2310 fix-loop E2, from the loop-D review) A command that FINISHES
    /// but leaves something holding its stdout pipe has its process group
    /// killed before the runner returns.
    ///
    /// The drain grace exists so a held-open pipe cannot hang the runner —
    /// but expiring it and walking away leaks whatever is holding the pipe.
    /// A `test_command` that starts a daemon then leaked one process PER
    /// GATED MOD, and paid a flat 2s for the privilege. The requirement is
    /// the kill; the time saving is a consequence.
    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn a_finished_command_that_leaves_a_process_holding_stdout_has_its_group_killed() {
        let m = marker(7);
        let started = Instant::now();
        let out = run_bounded(sh(&format!("sleep {m} & printf hi")), Duration::from_secs(600));
        let elapsed = started.elapsed();
        match out {
            Bounded::Finished { success, ref stdout, .. } => {
                assert!(success, "the command itself succeeded: {out:?}");
                assert_eq!(String::from_utf8_lossy(stdout), "hi", "its output is still captured in full");
            }
            ref other => panic!("expected Finished, got {other:?}"),
        }
        let left = survivors(&m);
        let _ = Command::new("pkill").args(["-f", &format!("sleep {m}")]).status();
        assert!(left.is_empty(), "the pipe-holding grandchild was leaked: {left}");
        // A bonus, not the requirement: with the group killed the drains
        // hit EOF instead of burning the whole grace.
        assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
    }

    /// (#2310 fix-loop E2, from the loop-D review) A configured `0` means
    /// UNBOUNDED, the way every other darkmux zero-knob does — not "kill
    /// instantly", which is what it meant before: `started.elapsed() >= 0`
    /// is true on the first poll, so a `0` in config.json turned every step
    /// command into a `TimedOut{0}` after about a millisecond.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS, a process-global
    fn a_configured_zero_is_unbounded_not_instant() {
        let k = "DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS";
        let prior = std::env::var(k).ok();
        std::env::set_var(k, "0");
        let timeout = configured_timeout();
        match &prior {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
        assert_eq!(timeout, UNBOUNDED, "zero disables the bound");
        // And a real command under it still runs to completion rather than
        // being killed on the first poll.
        match run_bounded(sh("printf hi"), timeout) {
            Bounded::Finished { success, stdout, .. } => {
                assert!(success);
                assert_eq!(String::from_utf8_lossy(&stdout), "hi");
            }
            other => panic!("a zero bound must not kill anything, got {other:?}"),
        }
    }

    /// A non-zero configured value is still a real bound — without this,
    /// making `configured_timeout` return `UNBOUNDED` unconditionally would
    /// leave the test above green.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS, a process-global
    fn a_configured_non_zero_is_still_a_real_bound() {
        let k = "DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS";
        let prior = std::env::var(k).ok();
        std::env::set_var(k, "7");
        let timeout = configured_timeout();
        match &prior {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
        assert_eq!(timeout, Duration::from_secs(7));
    }

    #[test]
    #[serial_test::serial]
    fn a_command_that_cannot_be_spawned_says_so() {
        let out = run_bounded(
            Command::new("darkmux-no-such-binary-2361"),
            Duration::from_secs(5),
        );
        assert!(matches!(out, Bounded::SpawnFailed(_)), "got {out:?}");
    }
}
