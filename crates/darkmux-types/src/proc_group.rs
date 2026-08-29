//! Process-group isolation + reaping (#2124).
//!
//! `darkmux mission launch review`'s remote seats shell out to `curl`
//! (`darkmux-crew`'s `remote_chat_attempt`) via a plain, blocking
//! `Command::new("curl").output()`. When the launcher itself is torn down
//! by SIGTERM/SIGINT while a probe/judge/verify call is in flight, that
//! `curl` child is a DIRECT OS child of the launcher process — nothing
//! about the signal delivery kills it on its own; an unhandled SIGTERM
//! kills the parent immediately and orphans the child, which then keeps
//! running (reparented to the OS's init process) until its OWN `-m`
//! timeout expires, sometimes minutes later. That's the "the remote-call
//! `curl` children that survived the parent" half of #2124.
//!
//! The fix is the standard POSIX one: make the launcher process the leader
//! of its OWN process group ([`become_group_leader`]), so every child it
//! spawns afterward — however deep the call stack that spawns it — shares
//! that SAME process group id (a plain `Command::new(...)` never overrides
//! its own group, and nothing in this codebase does), then, once a
//! terminal outcome is already durable on disk, send every process in that
//! group a `SIGKILL` in one shot ([`kill_group`]) — self included. That
//! self-inclusion is deliberate, not an oversight: by the time a caller
//! reaches for [`kill_group`], it has already written whatever it needed
//! to write (see `darkmux`'s `review_finalize_guard` module), so ending
//! the launcher's own process the same instant its children die is exactly
//! the outcome an operator's `kill <pid>` asked for — no live `curl`
//! left behind, no separate "and now exit" step needed.
//!
//! **Why `setpgid`, not "just trust the ambient group":** without an
//! explicit call, a freshly spawned process inherits whatever process
//! group its OWN parent happens to be in — a shell's per-job group when
//! run interactively (safe: the shell keeps the terminal's controlling
//! group, we get our own), but potentially the SAME group as an ACP
//! session's parent process or a test harness when spawned some other
//! way. [`kill_group`] on an inherited-not-owned group could reach
//! siblings this process never spawned and never should touch. Calling
//! `setpgid(0, 0)` makes the launcher the leader of a FRESH group — when
//! it's already a job leader (the common interactive case), this is a
//! harmless no-op; when it isn't, it deterministically severs it from
//! whatever group it would otherwise have shared.

use std::io;

/// Re-exported so callers outside this crate (`darkmux`'s own
/// `review_finalize_guard`) can name the signal [`kill_group`] sends
/// without taking their own direct `libc` dependency just for one
/// constant.
pub const SIGKILL: i32 = libc::SIGKILL;

/// Make the calling process the leader of a brand-new process group,
/// distinct from whatever invoked it. Returns the new group id (equal to
/// this process's own pid — see the module doc). Best-effort by design:
/// a caller that can't isolate (e.g. it's already a session leader in some
/// unusual host) still gets a pgid back — its OWN current one, read via
/// `getpgrp()` — so [`kill_group`] still reaches whatever group the
/// process actually sits in rather than the caller having nothing to pass
/// it at all.
pub fn become_group_leader() -> io::Result<u32> {
    let ret = unsafe { libc::setpgid(0, 0) };
    if ret == 0 {
        return Ok(std::process::id());
    }
    let err = io::Error::last_os_error();
    // Fall back to whatever group we're already in — `getpgrp()` cannot
    // fail for a valid calling process (POSIX).
    let current = unsafe { libc::getpgrp() };
    if current > 0 {
        Ok(current as u32)
    } else {
        Err(err)
    }
}

/// Send `sig` to every process in the group led by `pgid` (`kill(-pgid,
/// sig)` — POSIX's process-GROUP form of `kill(2)`). Deliberately includes
/// the caller itself when the caller is a member of that group — see the
/// module doc for why that is the intended, not accidental, behavior for
/// this fix's use of `SIGKILL`. A negative/zero `pgid` is refused (would
/// otherwise mean "every process this user owns" or "my own group" via a
/// path this function's callers don't intend) — silently a no-op, matching
/// this module's best-effort-cleanup posture (there is nowhere left to
/// report a failure to at the point this is called).
pub fn kill_group(pgid: u32, sig: i32) {
    if pgid == 0 {
        return;
    }
    unsafe {
        libc::kill(-(pgid as libc::pid_t), sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not much to assert cross-platform-safely about the REAL process
    /// group of the test harness (CI runners vary), but the call must not
    /// panic and must hand back a positive pgid either way — the
    /// success-path `setpgid` return or the `getpgrp` fallback.
    #[test]
    fn become_group_leader_returns_a_positive_pgid() {
        let pgid = become_group_leader().expect("becoming a group leader (or reading the fallback) must not error");
        assert!(pgid > 0, "pgid must be positive: {pgid}");
    }

    /// `kill_group(0, ...)` must be a deliberate no-op, never "signal
    /// group 0" (which POSIX defines as the CALLER's own group — exactly
    /// the footgun this guard exists to avoid for an uninitialized/unknown
    /// pgid). Proven by NOT killing the test process: if this sent
    /// `SIGKILL` to our own group, the test binary would die before
    /// reaching the assertion below.
    #[test]
    fn kill_group_zero_is_a_no_op() {
        kill_group(0, libc::SIGKILL);
        // Reached only if the call above did nothing.
    }
}
