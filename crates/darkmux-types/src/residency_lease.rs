//! Machine-local residency-lease registry (#1487 PR2, part of the
//! reconcile-to-need residency arc).
//!
//! gestalt's planner (`darkmux-gestalt`) already knows how to reconcile
//! residency down to what's DESIRED (`AcquireScope::Exclusive`) — but it
//! plans on RESIDENCY (`lms ps`), never LIVENESS (what another darkmux
//! process is actively mid-dispatch on). A naive Exclusive reconcile run
//! from one command could therefore unload a model a CONCURRENT darkmux
//! command is using right now (e.g. a CI review runner overlapping a local
//! dispatch). This module is the missing liveness input: a machine-local
//! record of "which `darkmux:*` models is each live darkmux process
//! actively dispatching to," fed into gestalt's `AcquireOpts.pinned` (#1487
//! PR1) so a reconcile pass never touches a busy resident.
//!
//! # Location + format
//!
//! One JSON file per live process at `<darkmux-home>/residency/<pid>.lease`
//! — the same home-resolution convention as
//! [`crate::dispatch_liveness`]'s `<darkmux-home>/liveness/<pid>.log`
//! (honor `DARKMUX_HOME`, else `~/.darkmux`). Each file holds `{"pid":
//! <pid>, "models": ["darkmux:foo", ...]}` — the UNION of every concurrent
//! in-process holder's current set (see "same-process aggregation" below).
//!
//! # Same-process aggregation (#2651)
//!
//! A `darkmux acp` daemon process can have more than one dispatch in flight
//! at once — `src/acp.rs` tracks `in_flight` per session and runs each
//! `session/prompt` as its own spawned task, and `src/acp_panel.rs`'s
//! ephemeral-panel path reaches [`LeaseGuard::acquire`] via
//! `run_step_graph` → `run_local_waves` on its own `spawn_blocking` thread.
//! Two such dispatches overlapping in the SAME process therefore hold TWO
//! [`LeaseGuard`]s at once, both stamped with the SAME `std::process::id()`
//! — the file is keyed by pid, not by holder.
//!
//! [`LeaseGuard::write`] and `Drop` account for this: each guard is minted a
//! process-unique token at [`LeaseGuard::acquire`] and contributes its own
//! current model set to a process-wide in-memory registry (`ACTIVE_LEASES`,
//! keyed by token). The on-disk `<pid>.lease` file always reflects the
//! UNION of every token currently registered — a guard's own
//! [`LeaseGuard::write`] call replaces only ITS OWN contribution (still
//! wholesale, never a delta, from that one caller's perspective), and its
//! `Drop` removes only ITS OWN contribution, rewriting the file to the
//! remaining union (or deleting it once the registry is empty — no holder
//! left in this process). This is what makes two concurrent same-process
//! dispatches safe: neither's write clobbers the other's, and neither's
//! early completion deletes a lease the other still needs. See
//! [`ACTIVE_LEASES`]'s own doc for the locking discipline that keeps every
//! mutation — including one that unwinds through a panic — leave the
//! registry and the on-disk file consistent.
//!
//! # `lms ps` stays the truth
//!
//! This registry is a subordinate BUSY-OVERLAY, never authoritative for
//! residency. A lease naming a model that has since left `lms ps` (operator
//! manually unloaded it, a TTL fired, LMStudio restarted) is a harmless
//! no-op downstream: `darkmux_gestalt::AcquireOpts.pinned`'s consumer
//! (`plan_acquire`, #1487 PR1) already pins an identifier only when it is
//! BOTH `is_darkmux_owned` and present in `facts.residents`. So this module
//! does no residency intersection itself — it only tracks liveness.
//!
//! # pid-liveness, not pid+start-time
//!
//! [`live_leased_models`] treats a lease's pid as live via a `kill(pid, 0)`
//! probe (`ESRCH` = dead; anything else, including a permission error,
//! counts as alive). This is the CRASH backstop, not the primary release
//! mechanism — a clean process exit removes its own lease via
//! [`LeaseGuard`]'s `Drop`, so pid-liveness only matters for a lease whose
//! owner crashed (SIGKILL, power loss — nothing that runs a destructor).
//! Hardening against pid REUSE (stamping the process start-time alongside
//! the pid) is deliberately NOT built in v1: the failure direction of a
//! reused pid without it is a *missed* eviction (the stale lease still
//! reads as "live," so its named model stays pinned a little longer than
//! necessary) — never a *wrongful* one. Add start-time pinning here if that
//! slack ever proves to matter in practice.
//!
//! # The crash-orphan sweep rides the read path
//!
//! [`live_leased_models`] best-effort removes any lease file whose pid is
//! no longer alive as it scans — there is no separate sweep verb. A dead
//! process's lease is therefore reclaimed by the very next call any live
//! darkmux command makes to reconcile residency, with no cron/daemon
//! needed.

use crate::paths::expand_tilde;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseFile {
    pid: u32,
    models: Vec<String>,
}

/// The process-wide same-process-aggregation registry (#2651): every
/// currently-acquired [`LeaseGuard`] in THIS process contributes its own
/// current model set here, keyed by its own unique token — never by pid,
/// since every guard in one process shares the same pid. The on-disk
/// `<pid>.lease` file is always the union of every value in this map.
///
/// Both [`LeaseGuard::write`] and `Drop` hold this mutex across their WHOLE
/// critical section — map mutation AND the resulting file write/delete —
/// so two concurrent callers can never interleave a stale union onto disk;
/// whichever call finishes last always leaves the file matching the map's
/// state as of that call. `.lock().unwrap_or_else(PoisonError::into_inner)`
/// rather than a bare `.unwrap()`: nothing inside the critical section can
/// itself panic (map mutation + a `Result`-returning file write, no
/// arbitrary caller code runs while the lock is held), but recovering from
/// poisoning defensively means a hypothetical future panic elsewhere never
/// wedges every OTHER live holder's lease bookkeeping for the rest of the
/// process's life.
static ACTIVE_LEASES: LazyLock<Mutex<HashMap<u64, Vec<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static NEXT_LEASE_TOKEN: AtomicU64 = AtomicU64::new(1);

/// RAII guard releasing this holder's OWN contribution to the process's
/// residency lease on drop — the clean-exit release half of the contract
/// (the pid-liveness sweep in [`live_leased_models`] is the crash backstop
/// for when `Drop` never runs). Acquire ONCE per local track / standalone
/// dispatch near the start of the window during which it will call
/// [`LeaseGuard::write`], and hold it for that window's lifetime — a normal
/// return OR a panic-unwind through the holding scope both run `Drop`; only
/// a hard crash (SIGKILL, power loss) leaves the lease for the sweep to
/// reclaim.
///
/// Multiple `LeaseGuard`s may be live at once IN THE SAME PROCESS (#2651 —
/// see the module doc's "same-process aggregation" section): each is
/// tracked independently via its own token, so one guard's `write`/`Drop`
/// never clobbers or prematurely releases another's still-live contribution.
pub struct LeaseGuard {
    pid: u32,
    token: u64,
}

impl LeaseGuard {
    /// Acquire a guard for the CURRENT process, minting a token unique to
    /// this acquisition. Does not itself write a lease file — pair with one
    /// or more [`LeaseGuard::write`] calls while the guard is held.
    pub fn acquire() -> Self {
        Self { pid: std::process::id(), token: NEXT_LEASE_TOKEN.fetch_add(1, Ordering::SeqCst) }
    }

    /// Write (or overwrite) THIS guard's own contribution with `models` —
    /// the COMPLETE current set of `darkmux:*` identifiers IT is actively
    /// dispatching to, not a delta from its own last call. Never a delta
    /// against any OTHER concurrent guard's contribution either: the
    /// on-disk file is recomputed as the union across every currently-live
    /// guard's own wholesale set. Atomic (temp file + rename within the
    /// same directory) so a concurrent reader never observes a
    /// partially-written lease.
    pub fn write(&self, models: &[String]) -> Result<()> {
        let mut active = ACTIVE_LEASES.lock().unwrap_or_else(|poison| poison.into_inner());
        active.insert(self.token, models.to_vec());
        let union = union_of(&active);
        write_lease_file(self.pid, &union)
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let mut active = ACTIVE_LEASES.lock().unwrap_or_else(|poison| poison.into_inner());
        active.remove(&self.token);
        // The lock stays held (`active` is not dropped early) through
        // whichever file operation follows — matching `write`'s own
        // discipline (see `ACTIVE_LEASES`'s doc) so a concurrent `write`/
        // `Drop` on another guard can never interleave a stale result onto
        // disk between this map mutation and its corresponding file write.
        if active.is_empty() {
            let _ = remove_lease(self.pid);
        } else {
            let union = union_of(&active);
            let _ = write_lease_file(self.pid, &union);
        }
    }
}

/// Sorted, deduplicated union of every currently-registered holder's model
/// set — sorted so the on-disk file (and every test asserting its content)
/// is deterministic regardless of token-insertion order or `HashMap`
/// iteration order.
fn union_of(active: &HashMap<u64, Vec<String>>) -> Vec<String> {
    let mut union: Vec<String> = active.values().flatten().cloned().collect();
    union.sort();
    union.dedup();
    union
}

/// Write `models` to `<pid>.lease`, wholesale — the shared private I/O this
/// module's TWO writers ([`LeaseGuard::write`] and its `Drop`, both already
/// holding [`ACTIVE_LEASES`]'s lock when they call this) use, so the
/// serialize-atomic-rename mechanics live in exactly one place.
fn write_lease_file(pid: u32, models: &[String]) -> Result<()> {
    let dir = residency_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = lease_path(&dir, pid);
    let tmp = dir.join(format!("{pid}.lease.tmp"));
    let payload = LeaseFile { pid, models: models.to_vec() };
    let json = serde_json::to_string_pretty(&payload).context("serializing residency lease")?;
    fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("renaming residency lease into place at {}", path.display()))?;
    Ok(())
}

/// Every `darkmux:*` model id leased by an OTHER live process — i.e. every
/// `<pid>.lease` file in the registry whose pid is (a) not `own_pid` and
/// (b) still alive. A lease belonging to a dead pid is skipped from the
/// result AND best-effort removed (the crash-orphan sweep, folded into this
/// read — see module docs). A malformed or unreadable lease file is
/// skipped silently; this function never panics and never fails — a
/// registry read that can't be trusted degrades to "nothing pinned," which
/// is the same fail-open leniency `config.json` reads use elsewhere.
pub fn live_leased_models(own_pid: u32) -> Vec<String> {
    let dir = residency_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut models = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lease") {
            continue;
        }
        let Some(pid) = pid_from_path(&path) else { continue };
        if pid == own_pid {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else { continue };
        let Ok(lease) = serde_json::from_str::<LeaseFile>(&contents) else {
            continue; // malformed — skip, never panic
        };
        if !process_alive(pid) {
            let _ = fs::remove_file(&path); // best-effort crash-orphan sweep
            continue;
        }
        models.extend(lease.models);
    }
    models
}

fn pid_from_path(path: &Path) -> Option<u32> {
    path.file_stem()?.to_str()?.parse().ok()
}

fn lease_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.lease"))
}

fn remove_lease(pid: u32) -> Result<()> {
    let path = lease_path(&residency_dir(), pid);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// The registry directory: `<darkmux-home>/residency/`. Mirrors
/// [`crate::dispatch_liveness`]'s `liveness_dir` home resolution exactly
/// (honor `DARKMUX_HOME`, tilde-expanded, else `~/.darkmux`) so the two
/// per-process registries always agree on where "home" is.
fn residency_dir() -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    crate::env_audit::audit_env_read("DARKMUX_HOME");
    if let Ok(root) = std::env::var("DARKMUX_HOME") {
        let root = root.trim();
        if !root.is_empty() {
            return expand_tilde(root).join("residency");
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".darkmux").join("residency")
}

/// Is `pid` a live process? `kill(pid, 0)` sends no signal — it only probes
/// existence/permission. `ESRCH` (no such process) is the only "dead"
/// answer; anything else (success, or `EPERM` for a live process owned by
/// another user) counts as alive. This is the FAIL-SAFE direction named in
/// the module docs: a pid-reuse false positive here means a stale lease
/// reads as live a little longer than it should (a *missed* eviction,
/// never a *wrongful* one).
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if ret == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Non-POSIX fallback (darkmux's audit/flock substrate is already
/// POSIX-only, see `flock.rs`): with no portable liveness probe, fail safe
/// by assuming alive — a lease is never wrongfully reclaimed on an
/// unsupported platform, only possibly held a little longer than ideal.
#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Guards `DARKMUX_HOME` mutation the way `dispatch_liveness`'s tests
    /// do (`#[serial_test::serial]` on every test in this module handles
    /// the cross-test race; this helper handles restore-on-drop so a
    /// panicking assertion still leaves the env var as it found it).
    struct EnvGuard(Option<String>);
    impl EnvGuard {
        fn set(tmp: &Path) -> Self {
            let prev = std::env::var("DARKMUX_HOME").ok();
            unsafe { std::env::set_var("DARKMUX_HOME", tmp) };
            Self(prev)
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.0 {
                    Some(v) => std::env::set_var("DARKMUX_HOME", v),
                    None => std::env::remove_var("DARKMUX_HOME"),
                }
            }
        }
    }

    /// Spawn and immediately reap a trivial child process, returning its
    /// (now-dead) pid — a robust "definitely not alive" pid for tests,
    /// rather than a hand-picked large constant that risks colliding with
    /// a real running process.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawning a trivial child process");
        let pid = child.id();
        child.wait().expect("waiting for the trivial child to exit");
        pid
    }

    #[serial_test::serial]
    #[test]
    fn write_lease_then_read_by_a_different_own_pid_returns_the_models() {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::set(tmp.path());

        let guard = LeaseGuard::acquire();
        guard.write(&["darkmux:foo".to_string(), "darkmux:bar".to_string()]).unwrap();

        // Read as if from some OTHER process (own_pid deliberately wrong —
        // the real test process's pid, which genuinely IS alive, stands in
        // for "a live external process").
        let other_own_pid = std::process::id().wrapping_add(1);
        let mut models = live_leased_models(other_own_pid);
        models.sort();
        assert_eq!(models, vec!["darkmux:bar".to_string(), "darkmux:foo".to_string()]);
    }

    #[serial_test::serial]
    #[test]
    fn own_pid_is_excluded_from_the_read() {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::set(tmp.path());

        let guard = LeaseGuard::acquire();
        guard.write(&["darkmux:self-model".to_string()]).unwrap();

        let own_pid = std::process::id();
        let models = live_leased_models(own_pid);
        assert!(models.is_empty(), "a process must never see its own lease as pinned: {models:?}");
    }

    #[serial_test::serial]
    #[test]
    fn dead_pid_lease_is_ignored_and_swept() {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::set(tmp.path());
        let dir = residency_dir();
        fs::create_dir_all(&dir).unwrap();

        let pid = dead_pid();
        let path = lease_path(&dir, pid);
        let payload = LeaseFile { pid, models: vec!["darkmux:orphan".to_string()] };
        fs::write(&path, serde_json::to_string(&payload).unwrap()).unwrap();
        assert!(path.exists(), "precondition: the orphan lease file exists");

        let models = live_leased_models(std::process::id());
        assert!(models.is_empty(), "a dead pid's models must never come back pinned: {models:?}");
        assert!(!path.exists(), "the dead-pid lease must be best-effort swept on read");
    }

    #[serial_test::serial]
    #[test]
    fn malformed_lease_file_is_skipped_never_panics() {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::set(tmp.path());
        let dir = residency_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("99999999.lease"), b"not json at all {{{").unwrap();

        // No panic — that IS the assertion. A garbage file degrades to
        // "nothing pinned from this file," never a crash.
        let models = live_leased_models(std::process::id());
        assert!(models.is_empty());
    }

    #[serial_test::serial]
    #[test]
    fn lease_guard_removes_the_lease_on_drop() {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::set(tmp.path());

        let pid = std::process::id();
        let path = lease_path(&residency_dir(), pid);
        {
            let guard = LeaseGuard::acquire();
            guard.write(&["darkmux:held".to_string()]).unwrap();
            assert!(path.exists(), "the lease file exists while the guard is held");
        }
        assert!(!path.exists(), "the lease file is removed once the guard drops");
    }

    #[serial_test::serial]
    #[test]
    fn a_single_holders_second_write_replaces_its_own_contribution_not_a_delta() {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::set(tmp.path());

        let guard = LeaseGuard::acquire();
        guard.write(&["darkmux:a".to_string()]).unwrap();
        guard.write(&["darkmux:b".to_string()]).unwrap();

        let other_own_pid = std::process::id().wrapping_add(1);
        let models = live_leased_models(other_own_pid);
        assert_eq!(
            models,
            vec!["darkmux:b".to_string()],
            "a second write from the SAME guard replaces its own contribution wholesale, \
             never accumulates: {models:?}"
        );
    }

    #[serial_test::serial]
    #[test]
    fn live_leased_models_on_a_missing_directory_is_empty_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::set(&tmp.path().join("never-created"));

        let models = live_leased_models(std::process::id());
        assert!(models.is_empty());
    }

    // ── #2651: same-process aggregation ─────────────────────────────────
    //
    // The race this fixes: `darkmux acp` can have more than one ephemeral
    // panel dispatch in flight at once in the SAME process (`src/acp.rs`
    // tracks `in_flight` per session; `src/acp_panel.rs`'s dispatch path
    // reaches `LeaseGuard::acquire` via `run_step_graph` → `run_local_
    // waves`, each on its own `spawn_blocking` thread). Before this fix,
    // `write_lease` was a bare wholesale overwrite keyed only by
    // `std::process::id()` — with no per-holder identity, a second
    // concurrent guard's write silently clobbered the first's, and
    // whichever guard dropped FIRST deleted the shared per-pid file even
    // though the other was still actively dispatching. Confirmed against
    // the pre-fix code (captured in the PR): `write_lease(&["darkmux:a"])`
    // then `write_lease(&["darkmux:b"])` from two guards in one process
    // left ONLY `["darkmux:b"]` visible to a concurrent reader, and
    // dropping the first guard deleted the lease file outright.

    /// Two concurrent in-process holders must UNION their contributions —
    /// never clobber. Deleting the union step in `LeaseGuard::write` (i.e.
    /// writing only `models` for the current call instead of the union
    /// across `ACTIVE_LEASES`) reproduces the pre-fix clobber and fails
    /// this test's first assertion the same way the pre-fix code failed
    /// (`left: ["darkmux:b"], right: ["darkmux:a", "darkmux:b"]`).
    #[serial_test::serial]
    #[test]
    fn two_concurrent_in_process_holders_union_rather_than_clobber() {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::set(tmp.path());

        let guard_a = LeaseGuard::acquire();
        guard_a.write(&["darkmux:a".to_string()]).unwrap();

        let guard_b = LeaseGuard::acquire();
        guard_b.write(&["darkmux:b".to_string()]).unwrap();

        let other_own_pid = std::process::id().wrapping_add(1);
        let models = live_leased_models(other_own_pid);
        assert_eq!(
            models,
            vec!["darkmux:a".to_string(), "darkmux:b".to_string()],
            "two concurrent in-process holders must union their leases, never clobber each other"
        );

        // Direction 1 (a wrong fix that never releases): guard_a finishes
        // first (its dispatch completed) while guard_b is still actively
        // mid-dispatch. Dropping A must remove ONLY A's contribution, never
        // touch B's still-live one — a lease that outlives its work pins
        // memory forever, the exact failure this module exists to prevent.
        drop(guard_a);
        let models_after_a_drops = live_leased_models(other_own_pid);
        assert_eq!(
            models_after_a_drops,
            vec!["darkmux:b".to_string()],
            "dropping one concurrent holder must not delete another still-live holder's lease"
        );
        let path = lease_path(&residency_dir(), std::process::id());
        assert!(path.exists(), "the lease file must still exist while holder B remains active");

        // Direction 2 (a wrong fix that never deletes): once the LAST
        // holder drops, the file must actually go away — a lease that is
        // never released lets nothing evict it and pins the model forever.
        drop(guard_b);
        assert!(!path.exists(), "the lease file is removed once the LAST concurrent holder drops");
    }

    /// The panic path (mandatory per #2651's review): a holder that panics
    /// mid-dispatch (its wave/job panics while the lease guard is held,
    /// unwinding through `run_local_waves`) must still release ONLY its own
    /// contribution via `Drop` — never the whole file, and never a
    /// survivor's still-live lease. Proves both directions the operator
    /// named: the panicked holder's model is no longer wrongfully pinned
    /// (Drop still ran during unwind), and the survivor's live dispatch is
    /// never evicted by the panic (its own contribution is untouched).
    #[serial_test::serial]
    #[test]
    fn a_panicking_holder_releases_only_its_own_contribution_never_a_survivors() {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::set(tmp.path());

        let survivor = LeaseGuard::acquire();
        survivor.write(&["darkmux:survivor".to_string()]).unwrap();

        let unwound = std::panic::catch_unwind(|| {
            let doomed = LeaseGuard::acquire();
            doomed.write(&["darkmux:doomed".to_string()]).unwrap();
            panic!("simulated mid-dispatch panic (#2651 panic-path proof)");
        });
        assert!(unwound.is_err(), "precondition: the simulated panic must actually have unwound");

        let other_own_pid = std::process::id().wrapping_add(1);
        let models = live_leased_models(other_own_pid);
        assert_eq!(
            models,
            vec!["darkmux:survivor".to_string()],
            "a panicking holder's Drop must remove its own contribution (never pinning the \
             model forever) without touching a survivor's still-live lease (never evicting the \
             survivor's model mid-generation)"
        );

        let path = lease_path(&residency_dir(), std::process::id());
        assert!(path.exists(), "the survivor's lease file must still exist after the panic unwound");

        drop(survivor);
        assert!(!path.exists(), "the lease file is removed once the survivor itself drops");
    }
}
