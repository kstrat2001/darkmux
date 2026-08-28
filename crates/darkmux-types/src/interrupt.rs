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

use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_signum: libc::c_int) {
    // `Ordering::SeqCst` from inside a signal handler is unusual but safe
    // here: nothing else runs concurrently with this handler (single
    // OS-level interrupt), and the ONLY operation the handler performs is
    // this store — no allocation, no locking, no anything else the
    // async-signal-safety rules would forbid.
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Install the SIGINT handler for this process. Idempotent — calling it
/// more than once just re-installs the same handler. Safe to call from a
/// synchronous CLI entry point; replaces the default "kill the process"
/// SIGINT action with "set a flag the caller polls", so the caller decides
/// when and how to stop rather than the OS tearing it down mid-operation.
pub fn install() {
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
}
