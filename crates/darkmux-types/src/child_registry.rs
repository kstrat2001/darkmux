//! Process-wide registry of this launcher's own spawned child pids (#2124).
//!
//! **This module replaced an earlier, DISPROVEN approach.** The first cut
//! of #2124 isolated the launcher into its own process group
//! (`setpgid(0, 0)`) and, on a caught signal, `SIGKILL`ed the WHOLE group
//! — self included — once a terminal record was durable. Proven wrong by a
//! pty-based test before merge: `setpgid(0, 0)` is a no-op when a shell
//! already put the launcher in a fresh job-control group before `exec`
//! (the ordinary "run it at your prompt" case), but when darkmux is
//! instead a plain child of some OTHER foreground process — a
//! non-interactive wrapper SCRIPT (job control off), which inherits its
//! own pgid onto every command it runs without a `setpgid` of its own —
//! calling `setpgid(0, 0)` moves the launcher OUT of the terminal's
//! REGISTERED foreground process group. A real Ctrl-C then only reaches
//! the (now-orphaned) wrapper, never darkmux, which keeps running with an
//! Active mission and a live `curl` child — the exact failure #2124 exists
//! to fix, now caused by the fix itself. Measured directly: two pty
//! scenarios (`A`: darkmux exec'd directly as the pty's session leader;
//! `B`: a wrapper process forks darkmux as a plain child with no
//! `setpgid`) — Ctrl-C reached darkmux and finalized its mission in `A`,
//! and in `B` it killed the WRAPPER while darkmux ran on, orphaned, mission
//! left `active`. There is no cheap, purely-local way for darkmux to tell
//! these two shapes apart before deciding whether isolating its process
//! group is safe (checking `tcgetpgrp()` against its own `getpgrp()`
//! reports "yes, I'm currently the foreground group" in BOTH shapes,
//! since before isolating, darkmux either IS the sole member (`A`) or
//! SHARES the group with the wrapper (`B`) — the thing that would break is
//! invisible from inside the check).
//!
//! The fix that actually holds regardless of invocation shape: darkmux
//! **never changes its own process group**, so Ctrl-C delivery is
//! completely unaffected no matter how it was invoked. Instead, every
//! child process the review pipeline spawns (today: `curl`, via
//! `darkmux-crew`'s `remote_chat_attempt`) [`register`]s its pid the
//! moment it's spawned — before blocking on it — and [`deregister`]s once
//! it's been reaped. A signal-interrupted launcher calls [`kill_all`] to
//! reap exactly those pids by NUMBER, never a process group it doesn't
//! fully own.

use std::collections::BTreeSet;
use std::sync::Mutex;

static CHILDREN: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

/// Re-exported so callers outside this crate (`darkmux`'s own
/// `launch_guard` — renamed from `review_finalize_guard` in #2131, which
/// generalized this guard from review-only to all three `mission launch`
/// launchers) can name the signal [`kill_all`] sends without taking their
/// own direct `libc` dependency just for one constant.
pub const SIGKILL: i32 = libc::SIGKILL;

/// Companion to [`SIGKILL`], re-exported for the same reason: a caller
/// that wants to ask a process to stop CLEANLY (so its own RAII finalize
/// guard runs) rather than force it down should not have to take its own
/// `libc` dependency for one constant.
pub const SIGTERM: i32 = libc::SIGTERM;

/// `kill(2)`'s "no such process" errno, re-exported for the same reason
/// the signal numbers above are: it is the ONE [`kill_pid`] failure a
/// caller routinely wants to treat as "already gone, nothing to report"
/// rather than as news, and reaching for it should not cost that caller a
/// `libc` dependency (or a hardcoded `3`).
pub const ESRCH: i32 = libc::ESRCH;

/// Send `sig` to ONE pid the caller owns. Returns `Ok(())` if the kernel
/// accepted the request, `Err(std::io::Error)` otherwise (`ESRCH` when the
/// pid is gone, `EPERM` when it is not ours to signal) — unlike
/// [`kill_all`], the caller here has somewhere to report a failure to, and
/// a signal that silently failed to send is exactly the kind of thing an
/// operator must not have to guess at.
///
/// **Caller contract — pid ownership.** `pid` must be a process the caller
/// SPAWNED and has not yet reaped (`Child::try_wait`/`wait` returning
/// `Some` is the reap). An unreaped child that has already exited is a
/// zombie: it still holds its pid, so this call is a harmless no-op and
/// CANNOT reach an unrelated process. Pass a pid scavenged from `ps`/
/// `pgrep`, or one already reaped, and that guarantee is gone — the pid
/// may have been recycled by then, and this will signal a stranger.
///
/// Refuses every pid that `kill(2)` would read as something OTHER than one
/// process: `0` means "every process in my own process group", and any
/// NEGATIVE value means a process GROUP. A `u32` cannot be negative on its
/// own, but the `as libc::pid_t` cast every caller of `kill(2)` needs turns
/// anything above `i32::MAX` into one, so the range check is part of the
/// same guarantee rather than a separate paranoia. `Child::id()` never
/// produces either shape, so this only ever catches a caller that computed
/// the pid some other way.
pub fn kill_pid(pid: u32, sig: i32) -> std::io::Result<()> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing to signal pid {pid}: kill(2) would read that as a process GROUP, not one child"),
        ));
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Register a just-spawned child pid. Call BEFORE blocking on it (a
/// `Command::spawn()` + later `.wait()`/`.wait_with_output()`, never a
/// plain `.output()`, which offers no window to register the pid before
/// the call blocks).
pub fn register(pid: u32) {
    if let Ok(mut set) = CHILDREN.lock() {
        set.insert(pid);
    }
}

/// Deregister a child pid once it has been reaped (successfully or not),
/// so a long-lived process doesn't keep accumulating pids for children
/// that finished cleanly minutes or hours ago.
pub fn deregister(pid: u32) {
    if let Ok(mut set) = CHILDREN.lock() {
        set.remove(&pid);
    }
}

/// Send `sig` to every currently-registered child pid — best-effort (a
/// pid that already exited on its own reports `ESRCH`, silently ignored;
/// there is nowhere left to report a failure to at the point this is
/// called, matching every other cleanup call in this codebase's abort
/// paths). Snapshots the set before signaling so the loop itself never
/// holds the lock while calling into libc.
pub fn kill_all(sig: i32) {
    let pids: Vec<u32> = match CHILDREN.lock() {
        Ok(set) => set.iter().copied().collect(),
        Err(_) => return,
    };
    for pid in pids {
        unsafe {
            libc::kill(pid as libc::pid_t, sig);
        }
    }
}

/// Send `sig` to every currently-registered child pid EXCEPT those in
/// `excluded` — the same best-effort, snapshot-then-signal semantics as
/// [`kill_all`], but lets a caller carve out pids it has decided to
/// handle more gently (a targeted SIGTERM plus a bounded wait, never
/// escalated to this call's own signal) rather than reaching them with
/// the blanket sweep too.
///
/// (#2476 review round 2, MUST FIX 2) Added for `darkmux acp`'s shutdown
/// handler: a `mission launch` child holds its own `LaunchFinalizeGuard`
/// and its own registered docker/curl grandchildren, so SIGKILLing it
/// alongside every other registered dispatch child (the router/answerer
/// curl calls, which hold no such state) leaves the mission `active`
/// with no terminal record and orphans those grandchildren — this call
/// lets the caller reap everything else while leaving pids it has
/// already signaled more carefully untouched.
pub fn kill_all_except(sig: i32, excluded: &BTreeSet<u32>) {
    let pids: Vec<u32> = match CHILDREN.lock() {
        Ok(set) => set.iter().copied().filter(|pid| !excluded.contains(pid)).collect(),
        Err(_) => return,
    };
    for pid in pids {
        unsafe {
            libc::kill(pid as libc::pid_t, sig);
        }
    }
}

/// Test-only: empty the registry so back-to-back tests in the SAME
/// process (this global is process-wide, not per-test) don't contaminate
/// each other. Gated the same way `darkmux-types`'s other test-support
/// hooks are (`interrupt.rs`, `paths.rs`, `config_access.rs`).
#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_test() {
    if let Ok(mut set) = CHILDREN.lock() {
        set.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn register_deregister_round_trips() {
        reset_for_test();
        register(999_999);
        assert!(CHILDREN.lock().unwrap().contains(&999_999));
        deregister(999_999);
        assert!(!CHILDREN.lock().unwrap().contains(&999_999));
    }

    /// [`kill_pid`] must refuse pid 0 rather than pass it through to
    /// `kill(2)`, where it would mean "signal my ENTIRE process group" —
    /// i.e. this process and every sibling the shell put in the same job.
    /// Nothing else in this module can catch that: `libc::kill(0, SIGKILL)`
    /// is a perfectly valid call that returns success.
    #[test]
    fn kill_pid_refuses_pid_zero() {
        let err = kill_pid(0, SIGTERM).expect_err("pid 0 must be refused, never forwarded to kill(2)");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// The other end of the same guarantee: a `u32` above `i32::MAX` casts
    /// to a NEGATIVE `pid_t`, which `kill(2)` reads as a process group.
    /// Unreachable from `Child::id()`, which is exactly why nothing would
    /// notice if the check were dropped.
    #[test]
    fn kill_pid_refuses_a_pid_that_would_cast_negative() {
        let err = kill_pid(u32::MAX, SIGTERM).expect_err("a pid that casts negative must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// A pid that does not exist reports `ESRCH` as an `Err`, rather than
    /// being silently swallowed the way [`kill_all`] deliberately does —
    /// the whole reason this function returns a `Result` at all.
    #[test]
    fn kill_pid_reports_a_missing_process_instead_of_swallowing_it() {
        let err = kill_pid(999_999, SIGTERM).expect_err("a nonexistent pid must report an error");
        assert_eq!(err.raw_os_error(), Some(libc::ESRCH));
    }

    /// The live proof: a real child, signaled by pid through this function,
    /// actually dies. Without this, both tests above would stay green even
    /// if `kill_pid` never called `kill(2)` at all.
    #[test]
    fn kill_pid_actually_signals_a_real_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawning a sleep child");
        kill_pid(child.id(), SIGKILL).expect("signaling our own live child must succeed");
        let status = child.wait().expect("reaping the signaled child");
        assert!(!status.success(), "a SIGKILLed child must not report success: {status:?}");
    }

    #[test]
    #[serial_test::serial]
    fn kill_all_on_an_empty_registry_is_a_no_op() {
        reset_for_test();
        kill_all(SIGKILL); // must not panic; nothing registered to signal
    }

    /// A registered pid that does not correspond to a real process must
    /// not panic `kill_all` — `libc::kill` on a nonexistent pid just
    /// returns `ESRCH`, which this function deliberately ignores.
    #[test]
    #[serial_test::serial]
    fn kill_all_ignores_a_pid_that_does_not_exist() {
        reset_for_test();
        register(999_999); // extremely unlikely to be a real live pid
        kill_all(SIGKILL);
        deregister(999_999);
    }

    /// [`kill_all_except`]'s whole reason to exist: two real registered
    /// children, one named in `excluded` — the sweep must kill the
    /// OTHER one and leave the excluded one alive. RED-proved by hand:
    /// swapping this call for plain `kill_all` (ignoring `excluded`)
    /// makes the "excluded survives" assertion below fail, since both
    /// children would die.
    #[test]
    #[serial_test::serial]
    fn kill_all_except_spares_the_excluded_pid_and_kills_the_rest() {
        reset_for_test();
        let mut spared = std::process::Command::new("sleep").arg("30").spawn().expect("spawning the spared child");
        let mut reaped = std::process::Command::new("sleep").arg("30").spawn().expect("spawning the reaped child");
        let spared_pid = spared.id();
        let reaped_pid = reaped.id();
        register(spared_pid);
        register(reaped_pid);

        let excluded: BTreeSet<u32> = [spared_pid].into_iter().collect();
        kill_all_except(SIGKILL, &excluded);

        let reaped_status =
            reaped.wait_timeout_or_kill(std::time::Duration::from_secs(5), "kill_all_except's target");
        assert!(!reaped_status.success(), "the non-excluded child must be killed: {reaped_status:?}");

        // The spared child must still be alive — poll briefly rather than
        // asserting instantly, since a false pass here (checking before
        // `kill_all_except` could possibly have reached it) would prove
        // nothing.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            spared.try_wait().expect("polling the spared child").is_none(),
            "kill_all_except must not signal an excluded pid"
        );
        kill_pid(spared_pid, SIGKILL).expect("cleaning up the spared child");
        let _ = spared.wait();
        deregister(spared_pid);
        deregister(reaped_pid);
    }

    /// Small helper so the exclusion test above reads as an assertion,
    /// not a hand-rolled poll loop — used exactly once, kept local
    /// rather than promoted to a shared test util for one call site.
    trait WaitTimeoutOrKill {
        fn wait_timeout_or_kill(&mut self, timeout: std::time::Duration, label: &str) -> std::process::ExitStatus;
    }

    impl WaitTimeoutOrKill for std::process::Child {
        fn wait_timeout_or_kill(&mut self, timeout: std::time::Duration, label: &str) -> std::process::ExitStatus {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                if let Some(status) = self.try_wait().expect("polling a child") {
                    return status;
                }
                if std::time::Instant::now() >= deadline {
                    panic!("{label} did not exit within {timeout:?}");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}
