//! Shared RAII finalize guard for every `darkmux mission launch` launcher
//! (#2131), extracted from `review_finalize_guard.rs` (#2124/#2130) once a
//! second launcher (the crawl launcher, SIGINT-only — retired in #2301) and a third with NO
//! guard at all (`mission_launch.rs` — generic graphs + coder-phase) proved
//! the shape needed to be shared rather than reinvented per launcher.
//!
//! **Why a closure-parameterized guard, not one hardcoded to a Mission
//! envelope type.** The three launchers finalized completely differently —
//! the now-deleted dedicated review launcher wrote a `ReviewEnvelope`-derived
//! `MissionEnvelope`, the retired crawl launcher wrote a crawl summary + its own
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
//! the crawl launcher previously installed SIGINT only (this is the #2131
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
//! between-units polling seam (the crawl launcher's, retired in #2301) never needed to call it at
//! all — its own loop already stops cleanly once `close` runs.

#[cfg(test)]
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

/// (#2262) Reap-on-signal watchdog for a caller whose own dispatch has no
/// polling seam of its own to notice a caught signal mid-blocking-wait —
/// the shape `mission_launch.rs::launch` has always spawned inline for
/// itself (see its own comment at the spawn site), now shared so `darkmux
/// dispatch` and `darkmux lab run` — both single/looped dispatch callers
/// with no Task/Step graph of their own to poll from — get the same
/// coverage without re-deriving it.
///
/// **Why this is needed even though `arm()` alone looks sufficient.** The
/// docker/coder container dispatch path already self-kills on a caught
/// signal: `dispatch_internal.rs`'s trajectory tailer thread polls
/// `interrupt::is_set()` on its own ~250ms cadence and kills its
/// registered child pid once `arm()` has made that flag meaningful. But
/// the tool-less remote/hosted path (`remote_chat_attempt`'s `curl`) has
/// NO poll seam at all — it registers its child pid then blocks in a
/// single `child.wait_with_output()` with nothing watching it. Without a
/// watchdog like this one, `arm()` alone converts "SIGTERM kills the
/// process outright" into "SIGTERM sets a flag nothing ever reads for
/// that path" — the dispatch would hang until curl's own `-m
/// <timeout_seconds>` bound expired (`--timeout`, or 600s when it is
/// omitted — #2480 removed the flag's clap default, but this path's own
/// `unwrap_or(600)` fallback in `main.rs` is unchanged),
/// not "responds within a poll tick" the way every other signal-aware
/// path in this codebase does.
///
/// Returns a guard whose `Drop` stops the thread once the caller's own
/// dispatch is over — hold it for exactly the scope that needs
/// interruptibility, same as `mission_launch.rs`'s own
/// `WatchdogStopGuard`. Skipped entirely under `cfg(test)` (no unit test
/// needs a real background thread); a live signal-delivery proof spawns
/// the compiled binary as a subprocess instead, where `cfg!(test)` is
/// false regardless of how it was built.
pub(crate) fn spawn_reap_watchdog() -> WatchdogStopGuard {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let guard = WatchdogStopGuard(std::sync::Arc::clone(&stop));
    if !cfg!(test) {
        std::thread::spawn(move || {
            while !darkmux_types::interrupt::is_set() {
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            loop {
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                darkmux_types::child_registry::kill_all(darkmux_types::child_registry::SIGKILL);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
    }
    guard
}

/// RAII stop-flag for [`spawn_reap_watchdog`]'s background thread.
///
/// **NOT shared with `mission_launch.rs` — it still has its own inline
/// `WatchdogStopGuard` and its own inline spawn.** An earlier version of this
/// comment claimed the migration had happened; it has not, and saying so was
/// worse than the duplication, because it invited a reader to assume one
/// definition governs both. The two bodies were diffed and are semantically
/// identical, so migrating `mission_launch` onto this one is safe — but it is
/// its own change, kept out of #2262's diff so a working launcher was not
/// touched by a signal-handling fix. Until then these two must stay in sync by
/// discipline, which is exactly the reason to do the migration.
pub(crate) struct WatchdogStopGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for WatchdogStopGuard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
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
/// needed to call it at all (the retired crawl launcher's synchronous, self-polling
/// loop never does).
///
/// # When NOT to call this
///
/// (#2462 review) The repo carries two documented positions on this
/// function, and they are not in conflict once the deciding conditions are
/// named. `radio_cli.rs` REMOVED its call (#2477) and its comment gives
/// both reasons: the reap half was redundant, and the hard `exit` skipped
/// destructors that mattered there. Both reasons are load-bearing AT THAT
/// SITE and neither holds at the two #2462 sites:
///
/// * **Radio's child is deliberately NOT in `child_registry`** (its own
///   comment says so — the watchdog would `SIGKILL` it out from under its
///   `LaunchFinalizeGuard`), so the reap could never have reached it. The
///   `curl` child both #2462 sites block on IS registered, and their `Err`
///   can land inside the watchdog's own 100ms polling window — so the
///   synchronous `kill_all` here is the thing that guarantees the child is
///   dead before this process is.
/// * **Radio returns an exit code through `main`** and had a real
///   destructor in flight (`_synth`'s tempdir). At the #2462 sites the
///   dispatch call has already returned, so the ONLY live destructor the
///   exit skips is [`WatchdogStopGuard`]'s stop-flag store — whose entire
///   job ends with the process — and the alternative exit code is std's
///   generic `1`, which is precisely what the fix is trying not to report.
///
/// A site on the #2462 side of that line calls
/// [`report_reap_and_exit_on_signal`] rather than this function directly,
/// because the hard exit also skips `main`'s error printing.
pub(crate) fn reap_and_exit_on_signal() {
    if !darkmux_types::interrupt::is_set() {
        return;
    }
    darkmux_types::child_registry::kill_all(darkmux_types::child_registry::SIGKILL);
    std::process::exit(130);
}

/// [`reap_and_exit_on_signal`] for a call site whose only remaining output
/// is the `Err` it is holding — it PRINTS that error first, then reaps and
/// exits 130.
///
/// (#2462 review) `main` returns `anyhow::Result<()>`, so an `Err` that
/// propagates out of `run` is printed by std's own `Termination` impl as
/// `Error: {err:?}` (anyhow's `Debug`, i.e. the message plus its `Caused
/// by:` chain). `std::process::exit` runs BEFORE any of that. So a site
/// that force-exits on the way out of an error path throws the error text
/// away entirely: the operator gets exit 130 and nothing else on stderr,
/// and a `--json` orchestrator gets no diagnosis at all.
///
/// Measured on the real binary at both #2462 sites: with the exit call in
/// place, a SIGTERM mid-dispatch produced exit 130 and no error line; with
/// the call deleted, stderr carried `Error: step … dispatch.internal:
/// hosted dispatch interrupted by an operator signal …` — the exact
/// message #2462 exists to produce, discarded by #2462's own exit call.
/// Nothing caught it because both of the real-signal subprocess tests in
/// `tests/cli.rs` sent the child's stderr to `/dev/null`; they capture it
/// to a file and assert the text now.
///
/// Formats with `{err:?}` and the `Error: ` prefix DELIBERATELY — that is
/// byte-for-byte what std would have printed on the ordinary `?` return,
/// so a signal-interrupted run says exactly what a non-interrupted failure
/// says, plus the distinguishing exit code.
pub(crate) fn report_reap_and_exit_on_signal(err: &anyhow::Error) {
    if !darkmux_types::interrupt::is_set() {
        return;
    }
    eprintln!("Error: {err:?}");
    reap_and_exit_on_signal();
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

// (#2310 P4d) Test-only since the bespoke review launcher — its last
// production caller — retired.
#[cfg(test)]
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

    /// (#2131 review round 2, MUST-FIX 4 — cheap but real) Nothing pinned
    /// that [`arm`] actually WIRES UP SIGTERM/SIGHUP — every other test
    /// exercising them (this crate's own, plus `darkmux_types::interrupt`'s)
    /// calls `simulate_sigterm_for_test`/`simulate_sighup_for_test`, which
    /// invoke the handler function DIRECTLY and would stay green even if
    /// `arm()` had never called `install_term()`/`install_hup()` at all —
    /// exactly the gap that let reverting the crawl launcher to SIGINT-only
    /// leave 44/44 tests green. This test sends REAL OS signals (via
    /// `kill -TERM`/`kill -HUP` against this process's own pid — the root
    /// `darkmux` binary crate has no direct `libc` dependency to `raise(2)`
    /// with, so shelling out to the standard `kill(1)` utility is the
    /// dependency-free equivalent) after `arm()`, proving the installed
    /// handlers actually fire.
    /// (#2131 review round 2, NEW-5; round 4, F6) Panic-safe teardown for
    /// [`arm_installs_real_sigterm_and_sighup_handlers`] — `Drop` fires on
    /// EVERY exit from that test (a normal return, OR a failed
    /// `assert!` unwinding mid-test), so the real SIGINT/SIGTERM/SIGHUP
    /// handlers `arm()` installs on the real OS process (all three,
    /// even though this test only SENDS itself a real SIGTERM/SIGHUP)
    /// never survive to affect whichever test the harness runs next in
    /// the same process.
    /// `darkmux_types::interrupt::restore_default_for_test`'s own doc
    /// explains why `reset_for_test` alone (already called at each
    /// checkpoint below) isn't enough — that only clears this module's
    /// flag/counters, never the actual `signal(2)` disposition.
    struct RestoreSignalsGuard;

    impl Drop for RestoreSignalsGuard {
        fn drop(&mut self) {
            darkmux_types::interrupt::restore_default_for_test();
            darkmux_types::interrupt::reset_for_test();
        }
    }

    #[test]
    #[serial_test::serial]
    fn arm_installs_real_sigterm_and_sighup_handlers() {
        darkmux_types::interrupt::reset_for_test();
        let _restore = RestoreSignalsGuard;
        arm();
        let pid = std::process::id().to_string();

        assert!(
            std::process::Command::new("kill")
                .args(["-TERM", &pid])
                .status()
                .expect("kill(1) must be runnable in this test environment")
                .success(),
            "kill -TERM must succeed sending a real signal to this process"
        );
        assert!(
            wait_for_interrupt(),
            "arm() must install a REAL SIGTERM handler — is_set() never flipped after a real \
             SIGTERM was delivered"
        );
        darkmux_types::interrupt::reset_for_test();

        assert!(
            std::process::Command::new("kill")
                .args(["-HUP", &pid])
                .status()
                .expect("kill(1) must be runnable in this test environment")
                .success(),
            "kill -HUP must succeed sending a real signal to this process"
        );
        assert!(
            wait_for_interrupt(),
            "arm() must install a REAL SIGHUP handler — is_set() never flipped after a real \
             SIGHUP was delivered"
        );
        // `_restore`'s `Drop` (below, at end of scope) does the final
        // `reset_for_test` + the real `restore_default_for_test` this
        // NEW-5 fix is for.
    }

    /// A real signal lands asynchronously — `kill(1)` exiting only means
    /// the OS accepted the request, not that this process has run the
    /// handler yet. Poll briefly instead of asserting immediately.
    fn wait_for_interrupt() -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if darkmux_types::interrupt::is_set() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        darkmux_types::interrupt::is_set()
    }

    // (#2131 review round 2, MUST-FIX 3 — ported from the retired
    // `review_finalize_guard.rs`'s own test module, deleted, unported, by
    // #2131's extraction; `panic_message` itself moved here unchanged.)

    #[test]
    fn panic_message_reads_a_str_payload() {
        let payload: Box<dyn Any + Send> = Box::new("boom");
        assert_eq!(panic_message(&*payload), "boom");
    }

    #[test]
    fn panic_message_reads_a_string_payload() {
        let payload: Box<dyn Any + Send> = Box::new("boom".to_string());
        assert_eq!(panic_message(&*payload), "boom");
    }

    #[test]
    fn panic_message_names_an_unrecognized_payload_honestly() {
        let payload: Box<dyn Any + Send> = Box::new(42i32);
        assert_eq!(panic_message(&*payload), "unknown panic payload");
    }

    // (#2248) SETTLING the suspected hazard: "can `LaunchFinalizeGuard::
    // drop`'s ungated `kill_all(SIGKILL)` fire while a worker thread is
    // still inside a live dispatch, turning a genuinely in-progress
    // dispatch into a signal-killed one with no typed error to show for
    // it?"
    //
    // `Drop`'s `kill_all` (in `impl Drop for LaunchFinalizeGuard` above)
    // really is the only one of the (now six, not four — `child_registry::
    // kill_all` grew two more call sites since #2248 was filed:
    // `crates/darkmux-serve/src/lib.rs`'s `reap_dispatch_children_on_
    // shutdown` calls `interrupt::mark_interrupted()` immediately before
    // its own `kill_all`, and `crates/darkmux-crew/src/dispatch_internal.
    // rs`'s tailer loop is already inside an `if interrupt::is_set()`
    // block) `kill_all` call sites NOT gated on `interrupt::is_set()`. That
    // part of the issue is still accurate.
    //
    // What is NOT accurate — traced here, not assumed — is that this guard
    // can reach `Drop` while a dispatch is live. `LaunchFinalizeGuard` has
    // exactly ONE production construction site in the whole tree:
    // `mission_launch.rs:988`, inside `launch()`. That function calls
    // `darkmux-crew::scheduler::run_step_graph` exactly once, synchronously,
    // on `launch()`'s OWN thread — never spawns a worker thread of its own
    // to run it. `run_step_graph`'s wave loop (`scheduler.rs:1348-1407`)
    // runs each wave's dispatches inside `std::thread::scope`, blocking on
    // `worker.join()` BEFORE the scope call returns. `std::thread::scope`
    // guarantees every thread spawned inside it is joined before the scope
    // call itself returns — normally OR via a panic — so `launch()`'s stack
    // frame (where `guard` lives) cannot begin to unwind (the only way
    // `Drop` fires) until `run_step_graph` has already returned, which in
    // turn cannot happen until every worker thread of every wave has
    // already finished. A per-step panic doesn't even reach that boundary:
    // `concurrent_dispatch.rs:1082` catches it with `catch_unwind` and
    // converts it to a per-step `Err` (pinned by `scheduler.rs`'s own
    // `run_step_graph_panicking_step_persists_terminal_error_never_
    // running` test), so the worker thread itself never panics either.
    //
    // This pair of tests proves the mechanism with a REAL registered
    // subprocess (not a mock), rather than asserting it from reading alone
    // (per this codebase's own "inference stops at the boundary" doctrine
    // — a claim about thread scheduling is a claim about the runtime, not
    // about our own code, so it gets executed). The first test mirrors
    // `run_step_graph`'s actual shape (`thread::scope` + `.join()`) and
    // shows the guard's unconditional `Drop` kill finds nothing left alive
    // to kill. The second test is NOT the production shape — `run_step_
    // graph` never does this — it exists only to prove the first test is
    // MEANINGFUL: with a bare, unjoined `thread::spawn` in place of
    // `thread::scope`, the exact hazard #2248 asked about is real and
    // reproducible. The difference between the two tests is the entire
    // reason the hazard does not reach production.
    #[test]
    #[serial_test::serial] // `child_registry` is process-wide
    #[cfg(unix)]
    fn scoped_worker_join_means_drop_never_races_a_live_registered_child() {
        darkmux_types::child_registry::reset_for_test();

        let abort_calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let abort_calls_for_writer = std::sync::Arc::clone(&abort_calls);
        let mut child = std::process::Command::new("sleep")
            .arg("0.3")
            .spawn()
            .expect("spawn a real `sleep` child");
        let pid = child.id();
        darkmux_types::child_registry::register(pid);

        {
            let guard = LaunchFinalizeGuard::new(move || {
                abort_calls_for_writer.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });

            // The SAME shape `run_step_graph`'s wave loop uses
            // (`scheduler.rs:1348-1407`): the blocking wait happens on a
            // scoped worker thread, joined before the scope call returns.
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| child.wait());
                worker.join().expect("worker thread must not panic").expect("wait must not error");
            });
            darkmux_types::child_registry::deregister(pid);

            // By the time control reaches here — the only place `guard`
            // could possibly Drop in this test — the child has ALREADY
            // exited cleanly and been deregistered. This is the state
            // `launch()` is always in immediately after `run_step_graph`
            // returns: nothing left alive for a still-armed guard's
            // unconditional `kill_all` to reach.
            assert!(
                darkmux_types::child_registry::kill_pid(pid, 0).is_err(),
                "the child must already be gone before the guard could ever Drop — there is no \
                 window where a live dispatch and an unwinding guard coexist on this thread"
            );

            // Left armed on purpose (no `close()` call) — Drop's abort
            // writer still fires as the backstop it's meant to be; the
            // point of this test is that its `kill_all` has nothing left
            // to kill, not that Drop never runs at all.
            drop(guard);
        }

        assert_eq!(
            abort_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Drop's abort writer still fires as the backstop it's designed to be"
        );
        assert!(
            darkmux_types::child_registry::kill_pid(pid, 0).is_err(),
            "the child must still be gone after Drop's kill_all ran — confirms the kill was a \
             no-op, not a coincidence of timing"
        );
        darkmux_types::child_registry::reset_for_test();
    }

    /// NOT the production shape — `run_step_graph` never runs a dispatch
    /// this way. This test exists purely to prove the test above is
    /// meaningful: swap `thread::scope` + `.join()` for a bare, unjoined
    /// `thread::spawn` (a launcher that fires dispatch on a worker thread
    /// and does not wait for it before its own function can return/unwind)
    /// and the #2248 hazard is real — `LaunchFinalizeGuard::drop`'s
    /// unconditional `kill_all` reaches a child that is still genuinely
    /// doing work, not one that already finished. `run_step_graph`'s choice
    /// to use `thread::scope` (proven above) is what keeps this codebase on
    /// the safe side of this line, not luck.
    #[test]
    #[serial_test::serial] // `child_registry` is process-wide
    #[cfg(unix)]
    fn an_unjoined_worker_thread_would_let_drop_kill_a_live_child_the_shape_scheduler_rs_avoids() {
        darkmux_types::child_registry::reset_for_test();

        let guard = LaunchFinalizeGuard::new(|| {});
        // Deliberately far longer than anything this test's own deadlines
        // below wait for — natural completion must NEVER be able to
        // masquerade as the guard's kill. If this ever raced its own
        // 30s natural exit, the assertions below would already have timed
        // out and failed long before that could happen.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn a real `sleep` child");
        let pid = child.id();
        darkmux_types::child_registry::register(pid);

        // Detached: nothing waits for this before the "launcher" below
        // considers itself done — the anti-pattern this file's own module
        // doc and `spawn_reap_watchdog`'s doc warn a launcher's dispatch
        // must never take.
        let _detached = std::thread::spawn(move || {
            let _ = child.wait();
        });

        // Give the child a real head start so it is genuinely still
        // working, not merely spawned.
        std::thread::sleep(std::time::Duration::from_millis(150));
        let child_was_genuinely_alive = darkmux_types::child_registry::kill_pid(pid, 0).is_ok();

        // `guard` drops HERE, still armed, while the `sleep 30` above is
        // only ~150ms into its run — exactly the live-dispatch window
        // #2248 asked whether the guard could observe.
        drop(guard);

        // Give the SIGKILL a moment to land — bounded well short of the
        // child's own 30s natural exit, so "dead" here can only mean the
        // guard's kill actually reached it, never a coincidence of timing.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut child_now_dead = false;
        while std::time::Instant::now() < deadline {
            if darkmux_types::child_registry::kill_pid(pid, 0).is_err() {
                child_now_dead = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // Best-effort cleanup regardless of what the assertions below find
        // — never leave a `sleep 30` running past this test.
        let _ = darkmux_types::child_registry::kill_pid(pid, darkmux_types::child_registry::SIGKILL);
        darkmux_types::child_registry::reset_for_test();

        assert!(
            child_was_genuinely_alive,
            "the child must have been genuinely alive/working before Drop — otherwise this test \
             proves nothing about a live-dispatch race"
        );
        assert!(
            child_now_dead,
            "Drop's kill_all must have reached a child that was still ~29.85s from finishing on \
             its own — this is the #2248 hazard, reproduced in the one shape that can exhibit it \
             (an unjoined worker thread); `run_step_graph` never takes this shape (see the \
             companion test above), which is why production never sees it"
        );
    }
}
