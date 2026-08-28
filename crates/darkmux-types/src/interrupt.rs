//! Process-wide SIGINT (Ctrl-C) flag, for a synchronous CLI loop that needs
//! to notice an interrupt BETWEEN units of work rather than mid-syscall.
//!
//! `libc` is already a dependency of this crate (`flock.rs`,
//! `residency_lease.rs`, `style.rs`); this module is the natural home for a
//! raw `SIGINT` handler rather than pulling a signal-handling crate into the
//! workspace for one caller (#1959 packet 2, `darkmux mission launch crawl`
//! — a sequential unit loop that checks this flag after each dispatch
//! returns and stops cleanly with `stopped_by: "interrupted"` rather than
//! being torn down mid-dispatch).
//!
//! Deliberately minimal: one flag, set once, never cleared. A process that
//! wants "handle Ctrl-C for this one operation" installs the handler, polls
//! [`is_set`] in its own loop, and exits — it does not need the flag to
//! reset for a second unrelated operation in the same process, and darkmux's
//! CLI is one-shot-per-invocation, so that limitation costs nothing today.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// How many SIGINTs this process has received since [`install`]. Exists
/// purely to drive the second-signal escape hatch below — nothing reads
/// the count itself.
static SIGINT_COUNT: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_sigint(_signum: libc::c_int) {
    // `Ordering::SeqCst` from inside a signal handler is unusual but safe
    // here: nothing else runs concurrently with this handler (single
    // OS-level interrupt), and the only operations the handler performs
    // are these atomic stores plus one more `signal(2)` call — no
    // allocation, no locking, no anything else the async-signal-safety
    // rules would forbid (`signal(2)` itself is on POSIX's async-signal-
    // safe list).
    INTERRUPTED.store(true, Ordering::SeqCst);
    let count = SIGINT_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
    if count >= 2 {
        // (#1959 merge-gate finding 13) A caller that never polls `is_set`
        // for some reason (a bug, a stuck loop upstream of the poll point)
        // would otherwise make Ctrl-C do NOTHING past the first press —
        // no escape hatch at all. Restoring the OS default disposition
        // here means a THIRD Ctrl-C kills the process the normal way,
        // same as any process that never installed a handler.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
    }
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

/// Whether SIGINT has been received since [`install`] was called. Never
/// resets — see the module doc.
pub fn is_set() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
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
