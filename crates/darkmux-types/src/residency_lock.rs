//! (#3509) The machine-wide residency lock: one `flock(2)` on
//! `<darkmux-home>/run/residency.lock` that every darkmux process takes
//! around a memory decision and what it does with it.
//!
//! Free memory is read live, and a start's memory pages in only after the
//! server or load begins, so two decisions made at the same moment both see
//! the same free figure and both go ahead (measured: two 30 GiB starts on
//! two endpoints admitted into 40 GiB free). The lock serializes them: a
//! caller holds it from the reading through the evictions to the moment its
//! start is spawned. Holding it to the end of a start (minutes, for a large
//! model) would serialize every start on the machine, so the lock is
//! released once the start has been spawned, and the start's need stays
//! counted through its lease's reservation instead
//! ([`crate::residency_lease::LeaseGuard::write_starting`]): every decision
//! subtracts the reservations of starts still in flight
//! ([`crate::residency_lease::LeaseGuard::reserved_by_others`]) from the free
//! figure it reads. A model that answers may not have paged its memory in
//! yet, so its holder keeps the reservation past the answer
//! ([`crate::residency_lease::LeaseGuard::mark_loaded_reserving`]) and
//! shrinks it as the memory pages in; it ends then, or when the holder drops
//! its lease.
//!
//! **The protocol, for every path that makes a model resident** (an engine
//! server's start, an LM Studio load):
//!
//! 1. [`acquire`] the lock first, before any per-endpoint lock (always this
//!    order, so two callers can never wait on each other). Take a
//!    per-endpoint lock under it only WITHOUT waiting: when it is taken (a
//!    start in flight there holds it until it answers), drop this lock,
//!    wait for the endpoint's, let it go, and start again from 1. Never hold
//!    this lock while waiting on anything another start holds.
//! 2. Under both, read the endpoint, free memory and the leases again, and
//!    decide: reuse what another start made in between, or evict.
//! 3. Write the lease with its reservation (`write_starting`), then spawn
//!    or begin the load.
//! 4. Drop this lock once the start is spawned, never later (a start takes
//!    minutes). When the model answers, keep its reservation until its
//!    memory pages in.
//!
//! A caller that reuses what is already resident decides nothing about
//! memory and need not take it. A dispatch waiting for a busy model
//! (`fleet.busy_policy` `queue`) takes it on each try, so waiters decide one
//! at a time.
//!
//! The lock is per open file, so two threads of one process that each
//! [`acquire`] it exclude each other as two processes do. POSIX-only, like
//! [`crate::flock`].

use anyhow::Result;
use std::path::PathBuf;

/// (#3511) THE reclaimable-memory figure every memory decision reads: the
/// bytes the OS could hand out now, its free pages plus its inactive
/// (reclaimable) cache, as the platform reads it (macOS `vm_stat`, Linux
/// `/proc/meminfo`'s `MemAvailable`). The engine path's fit decision and
/// doctor's `RAM headroom` row read it; the LM Studio path and the memory
/// ledger are to read it too, so a model judged to fit in one place fits in
/// the others. A free-pages-only figure undercounts what a load can take
/// and is not this one. `None` when the platform cannot say, which a
/// decision treats as unknown, never as room.
pub fn reclaimable_bytes() -> Option<u64> {
    reclaimable_bytes_on(darkmux_platform::current())
}

/// [`reclaimable_bytes`] as `platform` reads it, so a test can fake it.
pub fn reclaimable_bytes_on(platform: &dyn darkmux_platform::Platform) -> Option<u64> {
    platform.reclaimable_memory_bytes()
}

/// The lock, held until dropped.
pub struct ResidencyLock {
    _guard: crate::flock::FlockGuard,
}

/// `<darkmux-home>/run/residency.lock`.
pub fn lock_path() -> PathBuf {
    crate::paths::user_root_guarded().join("run").join("residency.lock")
}

/// Take the machine-wide residency lock, waiting for whoever holds it.
pub fn acquire() -> Result<ResidencyLock> {
    Ok(ResidencyLock { _guard: crate::flock::lock_exclusive(&lock_path())? })
}

/// (#3514) The lock when nobody holds it, `None` at once when someone does:
/// for a caller that need not wait (a reuse decides nothing about memory).
pub fn try_acquire() -> Result<Option<ResidencyLock>> {
    Ok(crate::flock::try_lock_exclusive(&lock_path())?.map(|g| ResidencyLock { _guard: g }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two holders in one process exclude each other: the second waits until
    /// the first is dropped.
    #[test]
    #[serial_test::serial]
    fn a_second_holder_waits_for_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", dir.path()) };
        let first = acquire().unwrap();
        assert!(lock_path().starts_with(dir.path()), "under the home: {}", lock_path().display());
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let _second = acquire().unwrap();
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(), "the second holder waits");
        drop(first);
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the second holder gets it once the first is dropped");
        waiter.join().unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }
}
