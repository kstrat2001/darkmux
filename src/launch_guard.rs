//! Shared RAII finalize guard for every `darkmux mission launch` launcher
//! (#2131), extracted from `review_finalize_guard.rs` (#2124/#2130) once a
//! second launcher (`crawl_launch.rs`, SIGINT-only) and a third with NO
//! guard at all (`mission_launch.rs` — generic graphs + coder-phase) proved
//! the shape needed to be shared rather than reinvented per launcher.
//!
//! **Why a closure-parameterized guard, not one hardcoded to a Mission
//! envelope type.** The three launchers finalize completely differently —
//! `mission_launch_review.rs` writes a `ReviewEnvelope`-derived
//! `MissionEnvelope`, `crawl_launch.rs` writes a crawl summary + its own
//! `mission_terminal_with_reasoning_and_payload` call, `mission_launch.rs`
//! writes either a gate banner (coder-phase, no finalize at all on the
//! happy path) or a generic `build_envelope`/`finalize_mission`. Rather
//! than generalize over a shared envelope TYPE (there isn't one), this
//! guard generalizes over a shared envelope-writing ACTION: each launcher
//! hands it a closure — "how to record the abort record" at construction,
//! "how to record whatever outcome I already know" at [`close`]. The guard
//! itself never touches a mission id, a store handle, or an envelope shape
//! directly; it only owns the ARM/DISARM state machine and the child-reap
//! call, so it stays exactly as reusable as `Drop` + a callback allows.
//!
//! **Signal handling: SIGINT + SIGTERM + SIGHUP, always.** [`arm`] installs
//! all three (`darkmux_types::interrupt`) — the #2124 pty-test finding that
//! motivated SIGHUP for review applies identically to every launcher: any
//! of them can run as a plain child of a non-interactive wrapper script,
//! and a Ctrl-C that tears down the wrapper's controlling terminal sends
//! SIGHUP to darkmux the same way regardless of which launcher is running.
//! `crawl_launch.rs` previously installed SIGINT only (this is the #2131
//! fix for that gap).
//!
//! **Reaping — by pid, never by process group.** Unchanged from
//! `review_finalize_guard.rs`'s own doc: `darkmux_types::child_registry`
//! tracks every child a dispatch spawns (today, `curl` — `darkmux-crew`'s
//! `remote_chat_attempt`) by pid, registered before the blocking wait. This
//! guard's `Drop` always reaps (best-effort, defensive default — a Drop
//! reached still armed means something unexpected happened, so assume a
//! child might still be alive). A launcher that runs its dispatch on a
//! SEPARATE worker thread it can't safely join on a caught signal (review;
//! now also `mission_launch.rs`'s generic-graph/coder-phase path) calls
//! [`reap_and_exit_on_signal`] explicitly once its own terminal record is
//! durable — see that function's own doc for why this is a launcher
//! decision, not something the guard forces unconditionally in `close`. A
//! launcher whose dispatch is a plain synchronous loop with its own
//! between-units polling seam (`crawl_launch.rs`) never needs to call it at
//! all — its own loop already stops cleanly once `close` runs.

use std::any::Any;

/// Install SIGINT + SIGTERM + SIGHUP handling — call ONCE, before minting
/// the mission this run's [`LaunchFinalizeGuard`] will cover. Idempotent
/// (the underlying `darkmux_types::interrupt` calls are), so it's safe even
/// if a future caller ends up invoking it more than once in the same
/// process. Deliberately does NOT touch this process's own group — see
/// `darkmux_types::child_registry`'s module doc for why an earlier version
/// of the review fix did and was proven wrong by a pty test.
pub(crate) fn arm() {
    darkmux_types::interrupt::install();
    darkmux_types::interrupt::install_term();
    darkmux_types::interrupt::install_hup();
}

/// Best-effort rendering of a caught `std::thread::JoinHandle::join()`
/// panic payload — the two shapes `std::panic!`/`.expect()`/`.unwrap()`
/// actually produce (`&'static str`, `String`); anything else names itself
/// honestly rather than guessing. Shared by every launcher that supervises
/// its dispatch on a worker thread (review; `mission_launch.rs`'s
/// generic-graph/coder-phase path).
pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// A launcher that ran its dispatch on a worker thread it deliberately
/// abandoned (a caught signal, and joining would block on the same
/// blocking call the signal is trying to escape) calls this ONCE its own
/// terminal record is already durable on disk (i.e., right after
/// [`LaunchFinalizeGuard::close`] returns). Reaps every registered child
/// pid and force-exits with the conventional signal-terminated code (130 —
/// 128 + SIGINT's own 2, reused for SIGTERM/SIGHUP too, matching
/// `review_finalize_guard.rs`'s precedent) — `SIGKILL`ing a child pid does
/// NOT end this process on its own, so the launcher must explicitly exit
/// here rather than relying on a self-inclusion side effect.
///
/// A no-op when no signal was observed (checks `darkmux_types::interrupt::
/// is_set()` itself) — safe to call unconditionally after a normal
/// completion; only a launcher that actually abandoned a worker thread
/// needs to call it at all (`crawl_launch.rs`'s synchronous, self-polling
/// loop never does).
pub(crate) fn reap_and_exit_on_signal() {
    if !darkmux_types::interrupt::is_set() {
        return;
    }
    darkmux_types::child_registry::kill_all(darkmux_types::child_registry::SIGKILL);
    std::process::exit(130);
}

/// RAII guard shared by every `darkmux mission launch` launcher (#2131):
/// armed right after a launcher mints its Mission/Phase records, so ANY
/// exit from that point forward — the normal [`close`] call, an early
/// `?`-return, a panic that unwinds past the point the guard was
/// constructed, or a caught SIGTERM/SIGINT/SIGHUP — leaves a matching
/// terminal record behind instead of a mission stuck `Active` forever.
///
/// `close` is the normal end-of-run path and disarms the guard so `Drop`
/// never double-finalizes; `Drop` is the last-resort net for every other
/// exit, using the `abort_writer` closure supplied at construction (which,
/// unlike `close`'s writer, can't know what actually happened — it writes
/// a generic "aborted before a terminal outcome was recorded" record, the
/// same shape every launcher's Drop path already used before this
/// extraction).
///
/// [`close`]: LaunchFinalizeGuard::close
pub(crate) struct LaunchFinalizeGuard<A: FnMut()> {
    armed: bool,
    abort_writer: A,
}

impl<A: FnMut()> LaunchFinalizeGuard<A> {
    /// `abort_writer` is called ONLY from `Drop`, and only if the guard is
    /// still armed at that point (i.e., `close` was never reached) — it
    /// must not assume any of the run's real outcome, since by definition
    /// something interrupted before that outcome was determined.
    pub(crate) fn new(abort_writer: A) -> Self {
        Self { armed: true, abort_writer }
    }

    /// The normal end-of-run path: disarms the guard (so `Drop` becomes a
    /// no-op) and runs `writer`, which already knows this run's real
    /// outcome (a clean success, a degenerate result, a hard error, or a
    /// synthesized error for a caught panic/signal the caller detected
    /// itself) — the guard has no opinion on what `writer` does, only that
    /// it runs exactly once. Returns whatever `writer` returns, so a
    /// launcher whose finalize call also computes an exit code (crawl) can
    /// still use `close` as its own function's tail expression.
    pub(crate) fn close<T>(&mut self, writer: impl FnOnce() -> T) -> T {
        self.armed = false;
        writer()
    }
}

impl<A: FnMut()> Drop for LaunchFinalizeGuard<A> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        (self.abort_writer)();
        // Same reasoning as `review_finalize_guard.rs`'s original Drop:
        // reaching `Drop` still armed means something already went wrong
        // in a way no more specific path handled, so the safe default is
        // "assume a child might still be alive and kill it by pid" rather
        // than deciding case by case. No `std::process::exit` here (unlike
        // `reap_and_exit_on_signal`) — `Drop` can fire mid-unwind from many
        // places, some of which (a test, a caller with more cleanup of its
        // own) must not have the whole process pulled out from under them.
        darkmux_types::child_registry::kill_all(darkmux_types::child_registry::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn close_disarms_so_drop_never_calls_the_abort_writer() {
        let abort_calls = RefCell::new(0u32);
        let close_calls = RefCell::new(0u32);
        {
            let mut guard = LaunchFinalizeGuard::new(|| {
                *abort_calls.borrow_mut() += 1;
            });
            guard.close(|| {
                *close_calls.borrow_mut() += 1;
            });
        }
        assert_eq!(*close_calls.borrow(), 1, "close's writer must run exactly once");
        assert_eq!(*abort_calls.borrow(), 0, "a disarmed guard must never invoke the abort writer on Drop");
    }

    #[test]
    fn drop_without_close_invokes_the_abort_writer_exactly_once() {
        let abort_calls = RefCell::new(0u32);
        {
            let _guard = LaunchFinalizeGuard::new(|| {
                *abort_calls.borrow_mut() += 1;
            });
            // Deliberately no `close()` call — the guard goes out of scope
            // here still armed, exercising `Drop`'s fallback.
        }
        assert_eq!(*abort_calls.borrow(), 1, "an un-closed guard must invoke the abort writer exactly once on Drop");
    }

    #[test]
    fn close_returns_the_writers_value() {
        let mut guard = LaunchFinalizeGuard::new(|| {});
        let value = guard.close(|| 42i32);
        assert_eq!(value, 42);
    }
}
