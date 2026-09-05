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
//! # Contention marker (for `darkmux doctor`)
//!
//! Every time `try_acquire` is DECLINED because a fresh, alive lock is
//! already held by a different pid, it best-effort overwrites a sibling
//! file, `host-sampler.contention.json`, naming the pid that was declined
//! and the pid that held the lock at that moment. `darkmux doctor`'s `host
//! sampler` check reads this to Warn "two live pids" — see that check's
//! own doc for the honesty limit: this can only ever record that a SECOND
//! acquisition attempt happened recently, never that a second emitter is
//! CURRENTLY also active (the loser, by construction, never emits).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

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

/// The contention side-channel's shape — see the module doc's "Contention
/// marker" section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentionInfo {
    pub declined_pid: u32,
    pub declined_owner: String,
    pub observed_holder_pid: u32,
    pub ts_ms: u64,
}

/// RAII guard for a held lock. Dropping it releases the lock (best-effort,
/// only if this pid still owns it on disk) — covers a clean return, an
/// early `?`-return, and a panic-unwind through the holding scope; only a
/// hard crash (SIGKILL, power loss) leaves it for the next `try_acquire`'s
/// staleness/dead-pid reclaim to find.
pub struct SamplerLockGuard {
    pid: u32,
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

fn contention_path() -> PathBuf {
    lock_path().with_file_name("host-sampler.contention.json")
}

/// Best-effort read of the current lock state. `None` on a missing,
/// unreadable, or malformed file — every caller treats absence as "no
/// sampler active," never as an error.
pub fn read_lock() -> Option<LockState> {
    let text = fs::read_to_string(lock_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Best-effort read of the contention marker. `None` when no acquisition
/// attempt has ever been declined (or the marker is missing/malformed).
pub fn read_contention() -> Option<ContentionInfo> {
    let text = fs::read_to_string(contention_path()).ok()?;
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
    let tmp = dir.join(format!("host-sampler.lock.{}.tmp", state.pid));
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

/// Best-effort overwrite of the contention marker. Failures are swallowed
/// — this is a diagnostic side-channel for `darkmux doctor`, never
/// load-bearing for correctness.
fn record_contention(declined_pid: u32, declined_owner: &str, observed_holder_pid: u32) {
    let info = ContentionInfo {
        declined_pid,
        declined_owner: declined_owner.to_string(),
        observed_holder_pid,
        ts_ms: epoch_ms(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&info) {
        let path = contention_path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(path, json);
    }
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
/// holds it (and best-effort records contention for `darkmux doctor`).
pub fn try_acquire(owner: &str, interval_ms: u64) -> Option<SamplerLockGuard> {
    try_acquire_with_pid(std::process::id(), owner, interval_ms)
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
    try_acquire_with_pid(my_pid, owner, interval_ms)
}

fn try_acquire_with_pid(my_pid: u32, owner: &str, interval_ms: u64) -> Option<SamplerLockGuard> {
    if let Some(existing) = read_lock() {
        if existing.pid != my_pid {
            let alive = pid_alive(existing.pid);
            let stale = is_stale(&existing, epoch_ms());
            if alive && !stale {
                record_contention(my_pid, owner, existing.pid);
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
        Some(after) if after.pid == my_pid => Some(SamplerLockGuard { pid: my_pid }),
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
    fn a_second_acquire_against_a_fresh_lock_is_declined_and_records_contention() {
        with_isolated_home(|| {
            let guard = try_acquire("daemon", 5000).expect("first acquire succeeds");
            assert!(read_contention().is_none(), "no contention yet");
            // Simulate a SECOND process (distinct pid) attempting to
            // acquire the same fresh lock — see `try_acquire_as_for_test`'s
            // doc for why a real second pid can't be produced in-process.
            let other_pid = std::process::id().wrapping_add(1);
            let second = try_acquire_as_for_test(other_pid, "dispatch", 5000);
            assert!(second.is_none(), "a fresh lock held by a different (alive) pid is not stealable");
            let c = read_contention().expect("contention recorded");
            assert_eq!(c.declined_pid, other_pid);
            assert_eq!(c.declined_owner, "dispatch");
            assert_eq!(c.observed_holder_pid, std::process::id());
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
