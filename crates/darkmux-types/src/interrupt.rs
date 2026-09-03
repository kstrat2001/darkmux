//! Process-wide SIGINT (Ctrl-C) / SIGTERM / SIGHUP flag, for a synchronous
//! CLI loop that needs to notice an interrupt BETWEEN units of work rather
//! than mid-syscall.
//!
//! `libc` is already a dependency of this crate (`flock.rs`,
//! `residency_lease.rs`, `style.rs`); this module is the natural home for a
//! raw signal handler rather than pulling a signal-handling crate into the
//! workspace for these callers (#1959 packet 2, `darkmux mission launch
//! crawl` — a sequential unit loop that checks this flag after each
//! dispatch returns and stops cleanly with `stopped_by: "interrupted"`
//! rather than being torn down mid-dispatch; #2124, `darkmux mission launch
//! review` — a supervisor thread polls this flag while the pipeline runs on
//! a worker thread, so a `kill <pid>` (SIGTERM) mid-probe still leaves a
//! terminal mission record instead of an orphaned Active mission; #2131
//! generalized both of these — plus the third launcher, `darkmux mission
//! launch` for generic graphs + coder-phase, which had NO signal handling
//! at all before — onto one shared `darkmux`-crate guard,
//! `launch_guard::LaunchFinalizeGuard`, and every launcher now installs
//! ALL THREE signals via its `arm()`; see that module's own doc).
//!
//! **SIGHUP (#2124 pty-test finding).** `install_hup` exists because of a
//! measured, NOT hypothetical failure mode: when `darkmux mission launch
//! review` runs as a plain child of some OTHER foreground process sharing
//! its process group (a non-interactive wrapper SCRIPT, job control off —
//! darkmux deliberately never calls `setpgid` itself; see `darkmux_types::
//! child_registry`'s module doc for why), and that OTHER process is the
//! session leader holding the controlling terminal, a Ctrl-C that kills
//! the wrapper (no handler of its own) makes the kernel tear down the
//! session's controlling terminal — which sends SIGHUP to every SURVIVING
//! member of the terminal's foreground process group, darkmux included.
//! Proven via a pty test: without a SIGHUP handler, darkmux died silently
//! (unhandled SIGHUP's default disposition is terminate) with its mission
//! left `active` — no different from the original #2124 bug, just a new
//! trigger. `install_hup` closes that gap the same cooperative way
//! SIGINT/SIGTERM already do.
//!
//! SIGINT, SIGTERM, and SIGHUP share ONE flag ([`INTERRUPTED`]) — a caller
//! that wants "stop cleanly on any of them" polls [`is_set`] once,
//! regardless of which signal arrived; a caller that only cares about one
//! would simply never call [`install_term`]/[`install_hup`] — though as of
//! #2131 every `darkmux mission launch` launcher (crawl included, which
//! installed SIGINT only before that fix) calls all three via
//! `launch_guard::arm()`, so this degrees-of-freedom note is aspirational
//! for a FUTURE caller today, not a description of a current one. Each
//! signal keeps its OWN escalation counter so the
//! "two presses, then the OS default disposition comes back" escape hatch
//! (see [`on_sigint`]'s doc) works independently per signal — a SIGINT
//! then a SIGTERM is two DIFFERENT first deliveries, not one signal's
//! second.
//!
//! Deliberately minimal: one flag, set once, never cleared. A process that
//! wants "handle Ctrl-C/TERM for this one operation" installs the
//! handler(s), polls [`is_set`] in its own loop, and exits — it does not
//! need the flag to reset for a second unrelated operation in the same
//! process, and darkmux's CLI is one-shot-per-invocation, so that
//! limitation costs nothing today.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// How many SIGINTs this process has received since [`install`]. Exists
/// purely to drive the second-signal escape hatch below — nothing reads
/// the count itself.
static SIGINT_COUNT: AtomicU32 = AtomicU32::new(0);

/// (#2124) How many SIGTERMs this process has received since
/// [`install_term`] — the SIGTERM twin of [`SIGINT_COUNT`], counted
/// separately so the two signals' escalation ladders don't interfere.
static SIGTERM_COUNT: AtomicU32 = AtomicU32::new(0);

/// (#2124) SIGHUP twin of [`SIGTERM_COUNT`] — see this module's own doc
/// for why SIGHUP needs the same treatment.
static SIGHUP_COUNT: AtomicU32 = AtomicU32::new(0);

/// Shared handler body for both signals: set the flag, bump `count`, and
/// restore the OS default disposition for `signum` on the second delivery
/// — see [`on_sigint`]'s doc for why. `Ordering::SeqCst` from inside a
/// signal handler is unusual but safe here: nothing else runs concurrently
/// with this handler (single OS-level interrupt), and the only operations
/// performed are these atomic stores plus one more `signal(2)` call — no
/// allocation, no locking, no anything else the async-signal-safety rules
/// would forbid (`signal(2)` itself is on POSIX's async-signal-safe list).
fn deliver(signum: libc::c_int, count: &AtomicU32) {
    INTERRUPTED.store(true, Ordering::SeqCst);
    let n = count.fetch_add(1, Ordering::SeqCst) + 1;
    if n >= 2 {
        // (#1959 merge-gate finding 13) A caller that never polls `is_set`
        // for some reason (a bug, a stuck loop upstream of the poll point)
        // would otherwise make the signal do NOTHING past the first
        // delivery — no escape hatch at all. Restoring the OS default
        // disposition here means a THIRD delivery kills the process the
        // normal way, same as any process that never installed a handler.
        unsafe {
            libc::signal(signum, libc::SIG_DFL);
        }
    }
}

extern "C" fn on_sigint(signum: libc::c_int) {
    deliver(signum, &SIGINT_COUNT);
}

/// (#2124) SIGTERM twin of [`on_sigint`] — same shared-flag, own
/// escalation counter.
extern "C" fn on_sigterm(signum: libc::c_int) {
    deliver(signum, &SIGTERM_COUNT);
}

/// (#2124) SIGHUP twin of [`on_sigint`] — same shared-flag, own
/// escalation counter. See this module's own doc for why SIGHUP needs a
/// handler at all.
extern "C" fn on_sighup(signum: libc::c_int) {
    deliver(signum, &SIGHUP_COUNT);
}

/// Install the SIGINT handler for this process. Idempotent — calling it
/// more than once just re-installs the same handler and resets the
/// signal count, so a caller that (re)installs before starting a new
/// operation gets the full two-chances-then-default behavior again. Safe
/// to call from a synchronous CLI entry point; replaces the default "kill
/// the process" SIGINT action with "set a flag the caller polls", so the
/// caller decides when and how to stop rather than the OS tearing it down
/// mid-operation — until a SECOND signal arrives, at which point the
/// default disposition comes back (see [`on_sigint`]'s doc) so a third
/// press still works as an unconditional kill.
pub fn install() {
    SIGINT_COUNT.store(0, Ordering::SeqCst);
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
    }
}

/// (#2124) Install the SIGTERM handler for this process — the `kill <pid>`
/// signal (no `-9`), which defaults to killing the process outright. Same
/// idempotent, two-chances-then-default shape as [`install`]; a caller
/// that wants BOTH signals handled calls this AND [`install`].
pub fn install_term() {
    SIGTERM_COUNT.store(0, Ordering::SeqCst);
    unsafe {
        libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t);
    }
}

/// (#2124) Install the SIGHUP handler for this process — see this
/// module's own doc for the measured failure mode this closes (a
/// controlling terminal torn down out from under a surviving process in
/// the same foreground process group). Same idempotent,
/// two-chances-then-default shape as [`install`].
pub fn install_hup() {
    SIGHUP_COUNT.store(0, Ordering::SeqCst);
    unsafe {
        libc::signal(libc::SIGHUP, on_sighup as *const () as libc::sighandler_t);
    }
}

/// Whether SIGINT, SIGTERM, or SIGHUP has been received since [`install`]/
/// [`install_term`]/[`install_hup`] was called. Never resets — see the
/// module doc.
pub fn is_set() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Test-only: deliver a simulated SIGINT without installing a real signal
/// handler or sending a real OS signal — calls the internal handler
/// directly, exercising the exact same code path a real Ctrl-C would.
/// Gated the same way `darkmux-types`'s other test-support hooks are
/// (`cfg(any(test, feature = "test-support"))`, e.g. `paths.rs`/`config_
/// access.rs`) — unreachable from any production build. A caller in
/// ANOTHER crate (e.g. `darkmux`'s own launcher tests, which can't
/// see the private `on_sigint` fn directly) enables the `test-support`
/// feature on its dev-dependency to reach this.
#[cfg(any(test, feature = "test-support"))]
pub fn simulate_sigint_for_test() {
    on_sigint(libc::SIGINT);
}

/// (#2124) Test-only: deliver a simulated SIGTERM the same way
/// [`simulate_sigint_for_test`] delivers a simulated SIGINT.
#[cfg(any(test, feature = "test-support"))]
pub fn simulate_sigterm_for_test() {
    on_sigterm(libc::SIGTERM);
}

/// (#2124) Test-only: deliver a simulated SIGHUP the same way
/// [`simulate_sigint_for_test`] delivers a simulated SIGINT.
#[cfg(any(test, feature = "test-support"))]
pub fn simulate_sighup_for_test() {
    on_sighup(libc::SIGHUP);
}

/// Test-only: reset the flag and ALL THREE signal counts back to their
/// pre-[`install`]/[`install_term`]/[`install_hup`] state. `is_set` never
/// resets in production (see the module doc) — a caller using
/// [`simulate_sigint_for_test`]/[`simulate_sigterm_for_test`]/
/// [`simulate_sighup_for_test`] needs its own way to keep back-to-back
/// tests in the SAME process from contaminating each other via this
/// process-wide flag.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_test() {
    INTERRUPTED.store(false, Ordering::SeqCst);
    SIGINT_COUNT.store(0, Ordering::SeqCst);
    SIGTERM_COUNT.store(0, Ordering::SeqCst);
    SIGHUP_COUNT.store(0, Ordering::SeqCst);
}

/// (#2131 review round 2, NEW-5; round 4, F6) Test-only: restore SIGINT,
/// SIGTERM, and SIGHUP to their OS-default disposition. [`reset_for_test`]
/// above only resets THIS module's own flag/counters — it never touches
/// the real signal disposition, and neither do [`simulate_sigint_for_test`]/
/// [`simulate_sigterm_for_test`]/[`simulate_sighup_for_test`] (they call
/// the internal handler function directly, never `signal(2)`). A test
/// that instead calls [`install`]/[`install_term`]/[`install_hup`] (or,
/// one level up, `darkmux`'s own `launch_guard::arm`, which calls all
/// three together) to prove the REAL handler gets wired up needs this:
/// `signal(2)` dispositions are process-wide and persist across
/// `install*` calls until something changes them back, so skipping this
/// would leave a custom handler installed for every LATER test in the
/// same test binary process, whether or not that test itself calls
/// `install`/`install_term`/`install_hup` again. All three signals are
/// restored — even a test that only sends itself a real SIGTERM/SIGHUP
/// (never a real SIGINT, which could be disruptive to send to a live
/// test process) still installed a SIGINT handler as a side effect if it
/// went through `arm()`, which always calls [`install`] alongside the
/// other two.
#[cfg(any(test, feature = "test-support"))]
pub fn restore_default_for_test() {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::signal(libc::SIGHUP, libc::SIG_DFL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn is_set_reflects_a_raised_flag() {
        // Reset for test isolation — other tests in this binary may have
        // already set the process-wide flag via a real SIGINT delivery
        // (there won't be one in CI, but the store is here for hygiene).
        INTERRUPTED.store(false, Ordering::SeqCst);
        assert!(!is_set());
        on_sigint(libc::SIGINT);
        assert!(is_set());
        INTERRUPTED.store(false, Ordering::SeqCst);
    }

    /// (#1959 merge-gate finding 13) No real signals are sent — the
    /// handler function is called directly, twice, and the disposition is
    /// read back via `libc::signal`'s own "returns the PREVIOUS handler"
    /// contract (POSIX has no separate getter for a `signal(2)`-style
    /// handler): install `on_sigint` again and check what it replaced. If
    /// the returned previous handler is `SIG_DFL`, the process's real
    /// disposition had already been restored to default by the second
    /// `on_sigint` call under test.
    #[test]
    #[serial_test::serial]
    fn second_sigint_restores_default_disposition_so_a_third_ctrl_c_kills_normally() {
        INTERRUPTED.store(false, Ordering::SeqCst);
        SIGINT_COUNT.store(0, Ordering::SeqCst);
        install();

        on_sigint(libc::SIGINT);
        assert!(is_set(), "the first signal sets the flag");
        let after_first = unsafe { libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t) };
        assert_ne!(
            after_first,
            libc::SIG_DFL,
            "one signal must not yet restore the default disposition — the caller still gets \
             the chance to stop cleanly"
        );

        on_sigint(libc::SIGINT);
        let after_second = unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
        assert_eq!(
            after_second,
            libc::SIG_DFL,
            "a SECOND signal must restore SIG_DFL, so an operator whose caller never polls \
             is_set() still has a THIRD Ctrl-C that kills the process the normal way"
        );

        // Leave shared state clean for whichever test runs next.
        INTERRUPTED.store(false, Ordering::SeqCst);
        SIGINT_COUNT.store(0, Ordering::SeqCst);
    }
}
