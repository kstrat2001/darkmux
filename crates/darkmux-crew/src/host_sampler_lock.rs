//! Singleton coordination lock for the ONE machine-scoped host sampler
//! (#2413).
//!
//! Before this module, THREE things sampled the same host: a per-dispatch
//! `telemetry.process` emitter (2s, every dispatch), a per-dispatch
//! `machine.telemetry` emitter (same sampler, every dispatch), and the
//! daemon's own `host_sampler` ring feeding `/machine/resources` (never
//! wrote flow records). The operator's rule: "host state is machine-scoped,
//! sampled once per machine, and runs join to it by time." This module is
//! the coordination primitive that makes "once per machine" true across
//! process boundaries: exactly one process — normally `darkmux serve`'s
//! daemon, or the first dispatch process when no daemon runs — holds the
//! lock and is the machine's sole `machine.telemetry` emitter at any
//! moment.
//!
//! # Location + format
//!
//! One JSON file at `<darkmux-home>/liveness/host-sampler.lock` (see
//! `darkmux_types::config_access::host_sampler_lock_path`) — the SAME
//! `liveness/` directory `darkmux_types::dispatch_liveness` writes its
//! per-pid heartbeat floor into, because both are "who is alive/active on
//! this machine right now" coordination state, not operator config. Unlike
//! that floor's one-file-per-pid shape, this is a SINGLE file: `{"pid":
//! <pid>, "machine_uid": <string|null>, "started_ts_ms": <u64>,
//! "heartbeat_ts_ms": <u64>, "interval_ms": <u64>, "owner": "daemon" |
//! "dispatch"}`, overwritten atomically (temp file + rename) on every
//! acquire and every heartbeat.
//!
//! # Acquire / steal / heartbeat / release
//!
//! [`try_acquire`] succeeds when the lock is ABSENT, STALE (heartbeat older
//! than [`STALE_MULTIPLIER`] times its own declared `interval_ms`), or
//! DEAD (its pid no longer exists, per a `kill(pid, 0)` probe — see
//! `darkmux_types::residency_lease`'s identical pattern, mirrored here
//! rather than reinvented). Otherwise it returns `None` — the caller does
//! not sample this tick (a dispatch: it emits nothing and relies on
//! whoever holds the lock; the daemon: it keeps its ring running for
//! `/machine/resources` but skips flow-record emission, see
//! `darkmux-serve`'s `host_sampler` module).
//!
//! [`SamplerLockGuard::heartbeat`] rewrites the file's `heartbeat_ts_ms`
//! (and `interval_ms`, in case the caller's resolved cadence changed) —
//! but ONLY if the file on disk still names THIS pid as owner. If another
//! process has since stolen the lock (a race during a steal — see below),
//! `heartbeat` returns `false` and the caller must stop emitting. Dropping
//! the guard removes the file, again ONLY if it still names this pid — a
//! guard that already lost the race to a stealer must not delete the
//! stealer's fresh lock.
//!
//! # The steal race, named honestly
//!
//! `try_acquire`'s "check absent/stale/dead, then write" is NOT a single
//! atomic compare-and-swap — two processes racing to steal the same stale
//! lock can both observe "stealable" and both write, with the later
//! `rename` winning. `try_acquire` closes MOST of this window with a
//! verify-after-write read-back (write, then re-read; if the file no
//! longer names our own pid, someone else won the race and we back off to
//! `None`) — but a race resolved in the other order (we read back
//! successfully, then a moment later something else overwrites us) is only
//! caught on our NEXT `heartbeat` call, not instantly. This is the same
//! fail-safe direction as `residency_lease`: the failure mode is a brief
//! double-emission window, never a wrongful mutual exclusion.
//!
//! # Contention marker — RETIRED (#2413 round 3, MF1)
//!
//! `try_acquire` used to best-effort record every declined attempt (a
//! `host-sampler.contention.json` sibling file naming the two pids) for
//! `darkmux doctor`'s "two live pids" Warn. That measured the WRONG thing:
//! a decline against a fresh, alive lock held by the DESIGNED sole emitter
//! (a daemon steadily holding it, or a dispatch that got there first) is
//! the correct, healthy steady state, not contention — and every
//! acquisition attempt, including a caller's very first one, is exactly
//! that in ordinary operation whenever a daemon already runs. Recording it
//! anyway meant every dispatch start under a running daemon wrote the
//! marker and doctor warned for the next 3x interval, reading a healthy
//! install as faulty. There is no cheap way to tell "an attempt declined
//! by the designed holder" apart from "a genuine race between two
//! processes that both think they should be the emitter" from the
//! declined side alone — the file-based lock can only ever show ONE
//! current holder either way, so the marker was never able to prove a
//! SECOND emitter was active regardless. The channel is deleted rather
//! than kept half-working; `darkmux doctor`'s `host sampler` check is
//! Pass / Warn(stale) / Warn(dead pid) only now.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Process-local guard against a same-process race (#2413 M1). The file
/// lock's identity is the OS pid — but `concurrent_dispatch.rs` runs
/// parallel units as THREADS in one process, so every thread shares the
/// same real pid. Without this, a second in-process `try_acquire` fell
/// through the `existing.pid == my_pid` branch in `try_acquire_with_pid`
/// and happily re-acquired, so every thread believed it owned the
/// sampler — and the first one's `Drop` deleted the lock file out from
/// under the rest, leaving zero emitters for the remainder of the run.
/// Only the REAL entry point (`try_acquire`) touches this flag; the
/// test-only `try_acquire_as_for_test` deliberately bypasses it, since it
/// exists specifically to simulate a genuinely DIFFERENT process sharing
/// this test binary's real pid.
static PROCESS_OWNS_LOCK: AtomicBool = AtomicBool::new(false);

/// A lock stays stale-eligible-for-stealing once its heartbeat is older
/// than this many times its own declared `interval_ms`. Matches the
/// issue's "heartbeat older than 3 intervals" wording exactly.
const STALE_MULTIPLIER: u64 = 3;

/// The on-disk (and in-memory read) shape of the lock file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockState {
    pub pid: u32,
    pub machine_uid: Option<String>,
    pub started_ts_ms: u64,
    pub heartbeat_ts_ms: u64,
    pub interval_ms: u64,
    /// `"daemon"` or `"dispatch"` — which kind of process holds it, for
    /// `darkmux doctor`'s message.
    pub owner: String,
}

/// RAII guard for a held lock. Dropping it releases the lock (best-effort,
/// only if this pid still owns it on disk) — covers a clean return, an
/// early `?`-return, and a panic-unwind through the holding scope; only a
/// hard crash (SIGKILL, power loss) leaves it for the next `try_acquire`'s
/// staleness/dead-pid reclaim to find.
pub struct SamplerLockGuard {
    pid: u32,
    /// Whether this guard is responsible for clearing
    /// [`PROCESS_OWNS_LOCK`] on drop — true only for guards minted by the
    /// real `try_acquire()` entry point (never the test-only
    /// `try_acquire_as_for_test`, which simulates a different process).
    owns_process_flag: bool,
}

impl SamplerLockGuard {
    /// Rewrite the lock's heartbeat (and `interval_ms`, in case the
    /// caller's resolved cadence changed since acquisition) — but only if
    /// the file on disk still names this guard's pid as owner. Returns
    /// `false` when it does not (lost the lock to a steal race): the
    /// caller must treat this as "stop emitting," matching `Drop`'s own
    /// ownership check so a lost guard never clobbers a stealer's fresh
    /// lock on either path.
    pub fn heartbeat(&self, interval_ms: u64) -> bool {
        let Some(mut state) = read_lock() else { return false };
        if state.pid != self.pid {
            return false;
        }
        state.heartbeat_ts_ms = epoch_ms_now();
        state.interval_ms = interval_ms;
        write_lock_state(&state).is_ok()
    }
}

impl Drop for SamplerLockGuard {
    fn drop(&mut self) {
        if let Some(state) = read_lock() {
            if state.pid == self.pid {
                let _ = fs::remove_file(lock_path());
            }
        }
        if self.owns_process_flag {
            PROCESS_OWNS_LOCK.store(false, Ordering::Release);
        }
    }
}

fn epoch_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Public alias — `darkmux doctor` and `darkmux-serve` both need "now, in
/// the same clock this module stamps records with," without reaching past
/// this module's own encapsulation of `SystemTime`.
pub fn epoch_ms_now() -> u64 {
    epoch_ms()
}

fn lock_path() -> PathBuf {
    darkmux_types::config_access::host_sampler_lock_path()
}

/// Best-effort read of the current lock state. `None` on a missing,
/// unreadable, or malformed file — every caller treats absence as "no
/// sampler active," never as an error.
pub fn read_lock() -> Option<LockState> {
    let text = fs::read_to_string(lock_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// `kill(pid, 0)` liveness probe — mirrors
/// `darkmux_types::residency_lease`'s identical pattern (not re-exported
/// from there because that module keeps it private; duplicating one
/// six-line fail-safe probe is cheaper than widening that crate's public
/// surface for it).
#[cfg(unix)]
fn pid_alive_raw(pid: u32) -> bool {
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if ret == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn pid_alive_raw(_pid: u32) -> bool {
    // Fail-safe: never wrongfully reclaim on an unsupported platform.
    true
}

/// Public: is `pid` a live process on this machine? See `pid_alive_raw`'s
/// doc for the exact semantics (fail-safe toward "alive").
pub fn pid_alive(pid: u32) -> bool {
    pid_alive_raw(pid)
}

/// Is `state`'s heartbeat old enough to be stolen? Pure — no clock read of
/// its own, so it's testable against any `now_ms`.
pub fn is_stale(state: &LockState, now_ms: u64) -> bool {
    let max_age_ms = state.interval_ms.max(1).saturating_mul(STALE_MULTIPLIER);
    now_ms.saturating_sub(state.heartbeat_ts_ms) > max_age_ms
}

/// Write `state` to the lock file atomically (temp file + rename within
/// the same directory), creating the directory if needed.
fn write_lock_state(state: &LockState) -> Result<()> {
    let path = lock_path();
    let dir = path.parent().context("lock path has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp = dir.join(format!("host-sampler.lock.{}.{}.tmp", state.pid, nonce));
    let json = serde_json::to_string_pretty(state).context("serializing host-sampler lock")?;
    fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("renaming lock into place at {}", path.display()))?;
    Ok(())
}

/// Test-only: write an arbitrary [`LockState`] verbatim, bypassing
/// acquisition — lets a test construct a stale/dead-pid fixture directly
/// rather than faking a clock. Gated on the `test-support` feature (this
/// crate's OWN tests get it automatically via `cfg(test)`; a downstream
/// crate's tests — `darkmux-doctor`'s `check_host_sampler` fixtures —
/// enable the feature as a dev-dependency).
#[cfg(any(test, feature = "test-support"))]
pub fn write_lock_state_for_test(state: &LockState) {
    let _ = write_lock_state(state);
}

/// Attempt to become the machine's sole host-sampler emitter. `owner` is
/// `"daemon"` or `"dispatch"` (used only for `darkmux doctor`'s message).
/// `interval_ms` is the caller's OWN resolved cadence, stamped into the
/// lock so a reader (doctor, or a later `is_stale` check) knows what
/// "stale" means for THIS holder.
///
/// Returns `Some(guard)` on success (lock was absent, stale, or the named
/// pid is dead) — release it via `Drop` when this process stops sampling.
/// Returns `None` when a different, alive, fresh-heartbeat pid already
/// holds it. (#2413 round 3 MF1) Every call — a caller's first attempt AND
/// its later opportunistic retries — is equally silent on decline: the
/// module doc's retired "Contention marker" section explains why a decline
/// here is the correct, healthy steady state rather than something worth
/// flagging.
pub fn try_acquire(owner: &str, interval_ms: u64) -> Option<SamplerLockGuard> {
    // Claim the process-local slot FIRST (#2413 M1) — a second thread in
    // this same process must be declined before it ever touches the file,
    // matching the file-lock's own "decline, don't merge" semantics.
    if PROCESS_OWNS_LOCK.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return None;
    }
    match try_acquire_with_pid(std::process::id(), owner, interval_ms) {
        Some(mut guard) => {
            guard.owns_process_flag = true;
            Some(guard)
        }
        None => {
            // The file-level acquire lost (e.g. a genuinely different,
            // alive process holds it) — release the process-local claim
            // we optimistically took so a later call in this process can
            // still try.
            PROCESS_OWNS_LOCK.store(false, Ordering::Release);
            None
        }
    }
}

/// Test-only: same as [`try_acquire`], but with an EXPLICIT `my_pid`
/// instead of the real `std::process::id()`. A single test binary is one
/// real OS process, so two `try_acquire()` calls in the same test always
/// carry the SAME real pid — unable to exercise the "a DIFFERENT process
/// already holds it" branch at all. This lets a test simulate a second
/// process by naming a distinct pid while still exercising the real
/// `pid_alive`/`is_stale` logic against the REAL lock-holder's pid on disk
/// (which — for a lock this same test process wrote — genuinely is alive).
#[cfg(any(test, feature = "test-support"))]
pub fn try_acquire_as_for_test(my_pid: u32, owner: &str, interval_ms: u64) -> Option<SamplerLockGuard> {
    // Deliberately bypasses PROCESS_OWNS_LOCK — this helper simulates a
    // DIFFERENT process (a distinct simulated pid) sharing the real test
    // binary's pid, so the real process's in-process singleton must not
    // apply to it.
    try_acquire_with_pid(my_pid, owner, interval_ms)
}

fn try_acquire_with_pid(my_pid: u32, owner: &str, interval_ms: u64) -> Option<SamplerLockGuard> {
    if let Some(existing) = read_lock() {
        if existing.pid != my_pid {
            let alive = pid_alive(existing.pid);
            let stale = is_stale(&existing, epoch_ms());
            if alive && !stale {
                return None;
            }
        }
        // else: absent-of-a-different-owner (existing.pid == my_pid,
        // extremely unlikely pid reuse aside) falls through to re-acquire,
        // same as a genuinely stale/dead lock.
    }
    let now = epoch_ms();
    let state = LockState {
        pid: my_pid,
        machine_uid: darkmux_hardware::machine_uid().map(str::to_string),
        started_ts_ms: now,
        heartbeat_ts_ms: now,
        interval_ms,
        owner: owner.to_string(),
    };
    if write_lock_state(&state).is_err() {
        return None;
    }
    // Verify-after-write: close most of the steal-race window (see module
    // doc) by re-reading immediately. If someone else's write landed after
    // ours, back off rather than proceed believing we hold it.
    match read_lock() {
        Some(after) if after.pid == my_pid => Some(SamplerLockGuard { pid: my_pid, owns_process_flag: false }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // `lock_path()` resolves through `DARKMUX_HOME` (env, process-global) —
    // every test in this module mutates it, so they must not run
    // concurrently with each other OR with any other test in this crate
    // that reads/writes `DARKMUX_HOME`. `#[serial_test::serial]` alone only
    // serializes within THIS file; a crate-wide named lock would be needed
    // for cross-file safety, but `darkmux-crew`'s existing convention
    // (grep `serial_test::serial` elsewhere in this crate) is per-file
    // serialization, so this matches it.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_isolated_home(f: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
        f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    #[test]
    fn a_second_in_process_acquire_with_the_real_pid_is_declined() {
        // (#2413 M1) Same-process parallel dispatches (concurrent_dispatch.rs
        // runs sibling units as THREADS in one process) all carry the SAME
        // real OS pid, so the file-lock's pid comparison alone can't tell
        // them apart. Before the process-local guard, a second `try_acquire`
        // in this process fell through the `existing.pid == my_pid` branch
        // and happily re-acquired, making every thread believe it owns the
        // sampler.
        with_isolated_home(|| {
            let first = try_acquire("daemon", 5000).expect("first acquire in this process succeeds");
            let second = try_acquire("dispatch", 5000);
            assert!(second.is_none(), "a second in-process acquire with the real pid must be declined");
            // The first holder's lock file is untouched by the declined attempt.
            let state = read_lock().expect("first holder's lock file still present");
            assert_eq!(state.pid, std::process::id());
            drop(first);
            assert!(read_lock().is_none(), "dropping the sole holder removes the file");
        });
    }

    #[test]
    fn releasing_the_first_in_process_holder_lets_a_later_acquire_succeed() {
        with_isolated_home(|| {
            let first = try_acquire("daemon", 5000).expect("first acquire succeeds");
            assert!(try_acquire("dispatch", 5000).is_none(), "declined while first still holds it");
            drop(first);
            let second = try_acquire("dispatch", 5000);
            assert!(second.is_some(), "the process-local guard is released on Drop, so a later acquire can succeed");
        });
    }

    #[test]
    fn acquire_when_absent_then_release_removes_the_file() {
        with_isolated_home(|| {
            assert!(read_lock().is_none(), "nothing written yet");
            let guard = try_acquire("daemon", 5000).expect("lock is free");
            let state = read_lock().expect("lock file now exists");
            assert_eq!(state.pid, std::process::id());
            assert_eq!(state.owner, "daemon");
            assert_eq!(state.interval_ms, 5000);
            drop(guard);
            assert!(read_lock().is_none(), "Drop released the lock");
        });
    }

    #[test]
    fn mf1_a_dispatch_first_attempt_against_a_daemon_held_fresh_lock_writes_no_contention_marker() {
        // (#2413 round 3 MF1) A decline by the DESIGNED holder (a daemon
        // steadily emitting) is the correct, healthy steady state — not
        // contention. Before this fix, the dispatch's very FIRST acquire
        // attempt (not just its opportunistic retry) used the recording
        // `try_acquire`, so every dispatch start under a running daemon
        // wrote a contention marker and `darkmux doctor` warned "two live
        // pids" for the next 3x interval, reading a healthy install as
        // faulty. `try_acquire` no longer records contention at all — the
        // channel is retired, per this issue's own "or retire the
        // channel" option — so the marker file must never appear.
        with_isolated_home(|| {
            let daemon_guard = try_acquire("daemon", 5000).expect("daemon acquires first");
            let dispatch_pid = std::process::id().wrapping_add(1);
            let declined = try_acquire_as_for_test(dispatch_pid, "dispatch", 5000);
            assert!(declined.is_none(), "a fresh daemon-held lock is not stealable by a live dispatch");
            let contention_marker = lock_path().with_file_name("host-sampler.contention.json");
            assert!(
                !contention_marker.exists(),
                "the contention channel is retired — a routine decline must never write it"
            );
            drop(daemon_guard);
        });
    }

    #[test]
    fn a_second_acquire_against_a_fresh_lock_is_declined_and_writes_no_contention_marker() {
        // (#2413 round 3 MF1) The contention channel is retired — see
        // `mf1_a_dispatch_first_attempt_against_a_daemon_held_fresh_lock_
        // writes_no_contention_marker` for the full rationale. This test
        // now just pins that a decline stays a plain `None`, nothing more.
        with_isolated_home(|| {
            let guard = try_acquire("daemon", 5000).expect("first acquire succeeds");
            // Simulate a SECOND process (distinct pid) attempting to
            // acquire the same fresh lock — see `try_acquire_as_for_test`'s
            // doc for why a real second pid can't be produced in-process.
            let other_pid = std::process::id().wrapping_add(1);
            let second = try_acquire_as_for_test(other_pid, "dispatch", 5000);
            assert!(second.is_none(), "a fresh lock held by a different (alive) pid is not stealable");
            let contention_marker = lock_path().with_file_name("host-sampler.contention.json");
            assert!(!contention_marker.exists(), "the retired channel must never write this file");
            drop(guard);
        });
    }

    #[test]
    fn a_stale_heartbeat_is_stolen() {
        with_isolated_home(|| {
            write_lock_state_for_test(&LockState {
                pid: std::process::id(),
                machine_uid: None,
                started_ts_ms: 0,
                heartbeat_ts_ms: 0,
                interval_ms: 1000,
                owner: "daemon".to_string(),
            });
            // heartbeat_ts_ms=0 vs now is far more than 3x1000ms old.
            let guard = try_acquire("dispatch", 5000);
            assert!(guard.is_some(), "a stale lock must be stealable");
        });
    }

    #[test]
    fn a_dead_pid_lock_is_stolen_even_with_a_fresh_heartbeat() {
        with_isolated_home(|| {
            let now = epoch_ms_now();
            write_lock_state_for_test(&LockState {
                pid: 999_999, // not us; almost certainly not alive
                machine_uid: None,
                started_ts_ms: now,
                heartbeat_ts_ms: now,
                interval_ms: 5000,
                owner: "daemon".to_string(),
            });
            let guard = try_acquire("dispatch", 5000);
            assert!(guard.is_some(), "a lock naming a dead pid must be stealable regardless of heartbeat freshness");
        });
    }

    #[test]
    fn heartbeat_refreshes_timestamp_and_returns_true_while_still_owner() {
        with_isolated_home(|| {
            let guard = try_acquire("daemon", 5000).unwrap();
            let before = read_lock().unwrap().heartbeat_ts_ms;
            std::thread::sleep(std::time::Duration::from_millis(5));
            assert!(guard.heartbeat(5000), "still the owner");
            let after = read_lock().unwrap().heartbeat_ts_ms;
            assert!(after >= before, "heartbeat_ts_ms moved forward (or held, on a fast clock)");
        });
    }

    #[test]
    fn heartbeat_returns_false_and_does_not_resurrect_after_being_stolen() {
        with_isolated_home(|| {
            let guard = try_acquire("daemon", 1000).unwrap();
            // Simulate this holder going stale, then a second (distinct-
            // pid) process stealing it.
            let mut stale = read_lock().unwrap();
            stale.heartbeat_ts_ms = 0;
            write_lock_state_for_test(&stale);
            let thief_pid = std::process::id().wrapping_add(1);
            let thief = try_acquire_as_for_test(thief_pid, "dispatch", 1000).expect("the stale lock is stealable");
            assert_ne!(read_lock().unwrap().pid, guard_pid_for_test(&guard));
            // The ORIGINAL guard's heartbeat must now report false, and
            // must NOT clobber the thief's fresh lock.
            assert!(!guard.heartbeat(1000), "the original owner lost the race");
            assert_eq!(read_lock().unwrap().pid, guard_pid_for_test(&thief), "the thief's lock is untouched");
            // Dropping the ORIGINAL (losing) guard must not delete the
            // thief's lock either.
            drop(guard);
            assert!(read_lock().is_some(), "the thief's lock survives the original owner's Drop");
        });
    }

    /// Test helper: guards intentionally expose no public pid accessor
    /// (callers have no legitimate use for it outside this module), so
    /// tests reach the private field directly.
    fn guard_pid_for_test(g: &SamplerLockGuard) -> u32 {
        g.pid
    }

    #[test]
    fn drop_only_removes_the_lock_when_still_owned() {
        with_isolated_home(|| {
            let guard = try_acquire("daemon", 5000).unwrap();
            // Someone else's write lands (simulating a race where the
            // verify-after-write above this guard's own creation would
            // have caught it, but a LATER race after acquisition succeeds
            // has not yet been observed via heartbeat).
            write_lock_state_for_test(&LockState {
                pid: 424_242,
                machine_uid: None,
                started_ts_ms: epoch_ms_now(),
                heartbeat_ts_ms: epoch_ms_now(),
                interval_ms: 5000,
                owner: "dispatch".to_string(),
            });
            drop(guard);
            let after = read_lock().expect("a lock still exists");
            assert_eq!(after.pid, 424_242, "the other process's lock must survive our Drop");
        });
    }

    #[test]
    fn is_stale_pure_boundary() {
        let state = LockState {
            pid: 1,
            machine_uid: None,
            started_ts_ms: 0,
            heartbeat_ts_ms: 10_000,
            interval_ms: 1000,
            owner: "daemon".to_string(),
        };
        assert!(!is_stale(&state, 10_000 + 3000), "exactly 3x is not yet stale");
        assert!(is_stale(&state, 10_000 + 3001), "one ms past 3x is stale");
    }
}
