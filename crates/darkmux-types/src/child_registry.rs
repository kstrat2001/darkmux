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
/// `review_finalize_guard`) can name the signal [`kill_all`] sends without
/// taking their own direct `libc` dependency just for one constant.
pub const SIGKILL: i32 = libc::SIGKILL;

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
}
