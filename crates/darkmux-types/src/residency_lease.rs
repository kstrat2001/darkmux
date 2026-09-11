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
//! <pid>, "models": ["darkmux:foo", ...], "loaded": ["darkmux:foo"]}` —
//! `models` is the UNION of every concurrent in-process holder's currently
//! DESIRED set (see "same-process aggregation" below); `loaded`
//! (`#[serde(default)]` — an older file with no key reads as "nothing
//! confirmed yet") is the subset each holder has actually confirmed
//! resident via [`LeaseGuard::mark_loaded`] — the intent-vs-in-use
//! distinction [`LeaseGuard::identifiers_i_should_lead`] (#2672) needs to
//! tell "this pin is another holder racing to acquire the same resource I
//! am" from "this pin is a holder already mid-generation on it."
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
//! **That made the on-disk FILE correct — it did not, by itself, make a
//! same-process sibling's LIVE dispatch safe from this process's own
//! reconcile.** [`live_leased_models`] excludes `own_pid` by construction
//! (see its own doc below), which is right for a single-dispatch process
//! but meant a caller using it alone (the bare free function) would never
//! see a same-process sibling's contribution — correctly written into the
//! on-disk union — as pinned. That gap (#2662 review, tracked as #2663)
//! is closed by [`LeaseGuard::all_live_leased_models`]: the
//! same-process-INCLUSIVE read every `AcquireOpts.pinned` computation
//! should use instead of the bare free function, unioning
//! [`live_leased_models`] with every OTHER currently-live guard's own
//! contribution to [`ACTIVE_LEASES`] (excluding the calling guard's own
//! token, so a guard never pins its own placement against itself).
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
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

/// `models` (unchanged name/shape from before #2672 — every existing
/// hand-written test/production lease JSON with no `loaded` key stays
/// valid) is the FULL desired set; `loaded` (`#[serde(default)]`, so an
/// older file / a lease from a pre-#2672 binary reads as "nothing
/// confirmed loaded yet") is the subset of `models` this holder has
/// actually confirmed resident — see `LeaseGuard::mark_loaded`'s doc for
/// why that distinction exists and how it's used.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseFile {
    pid: u32,
    models: Vec<String>,
    #[serde(default)]
    loaded: Vec<String>,
}

/// The process-wide same-process-aggregation registry (#2651): every
/// currently-acquired [`LeaseGuard`] in THIS process contributes its own
/// current model set here, keyed by its own unique token — never by pid,
/// since every guard in one process shares the same pid. The on-disk
/// `<pid>.lease` file is always the union of every value in this map.
///
/// One holder's contribution: the models it currently DESIRES (the full
/// set `write` was last called with) and the subset it has actually
/// confirmed LOADED via `mark_loaded` (#2672) — see that method's doc.
/// `loaded` is always a subset of `desired`; `write` prunes it defensively
/// on every call so a model dropped from `desired` never lingers as stale
/// "loaded" state.
#[derive(Debug, Clone, Default)]
struct LeaseEntry {
    desired: Vec<String>,
    loaded: Vec<String>,
}

/// Several accessors touch this mutex: [`LeaseGuard::write`] and
/// [`LeaseGuard::mark_loaded`] (both new in #2672) and `Drop` all hold it
/// across their WHOLE critical section — map mutation AND the resulting
/// file write/delete — so two concurrent callers can never interleave a
/// stale union onto disk; whichever call finishes last always leaves the
/// file matching the map's state as of that call. [`LeaseGuard::all_live_leased_models`]
/// (and [`LeaseGuard::identifiers_i_should_lead`]'s same-process half) are
/// read-only: they hold the lock only for their own map iteration + sort +
/// dedup (no file write), and call `live_leased_models`/`live_foreign_leases`
/// — the file-I/O pieces — BEFORE taking the lock, so the lock is never
/// held across I/O on any of these paths. A bare `.lock().unwrap_or_else(PoisonError::into_inner)`
/// rather than a bare `.unwrap()` is used throughout: nothing inside any
/// critical section can itself panic (map mutation, a `Vec` sort/dedup, and
/// a `Result`-returning file write — no arbitrary caller code runs while
/// the lock is held), but recovering from poisoning defensively means a
/// hypothetical future panic elsewhere never wedges every OTHER live
/// holder's lease bookkeeping for the rest of the process's life.
static ACTIVE_LEASES: LazyLock<Mutex<HashMap<u64, LeaseEntry>>> =
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
        {
            let entry = active.entry(self.token).or_default();
            entry.desired = models.to_vec();
            // A model no longer desired can't still be "loaded" (#2672) —
            // prune defensively rather than trust every caller to also call
            // `mark_loaded` again for a narrower re-`write`.
            entry.loaded.retain(|m| entry.desired.contains(m));
        }
        let (desired, loaded) = union_of(&active);
        write_lease_file(self.pid, &desired, &loaded)
    }

    /// (#2672) Mark `models` — a subset of THIS guard's own current
    /// `desired` set (anything not currently desired is ignored, never
    /// added) — as CONFIRMED resident: the "genuinely in use" phase, as
    /// opposed to merely "intends to acquire." Call exactly once
    /// [`crate::residency_lease`]'s caller (`ensure_wave_loaded`) has
    /// observed a successful `PlanExecOutcome::Loaded` for these models —
    /// never speculatively, and never before the host call actually
    /// returned success.
    ///
    /// This is the intent-vs-in-use distinction MUST FIX 1 (#2672) needs:
    /// [`LeaseGuard::identifiers_i_should_lead`] treats an identifier ANY
    /// live holder has marked `loaded` as an unconditional, un-overridable
    /// pin (#2669's original protection, completely untouched) — but an
    /// identifier only ever `write`-ed as DESIRED, never marked `loaded`,
    /// is a holder that is itself STILL racing to acquire the same
    /// resource, not one already mid-generation on it. Only ADDS to this
    /// guard's own `loaded` set (intersected with its current `desired`) —
    /// never removes an existing entry; a subsequent `write()` narrowing
    /// `desired` is what retires a stale `loaded` entry (see `write`'s own
    /// pruning above).
    pub fn mark_loaded(&self, models: &[String]) -> Result<()> {
        let mut active = ACTIVE_LEASES.lock().unwrap_or_else(|poison| poison.into_inner());
        {
            let entry = active.entry(self.token).or_default();
            for m in models {
                if entry.desired.contains(m) && !entry.loaded.contains(m) {
                    entry.loaded.push(m.clone());
                }
            }
        }
        let (desired, loaded) = union_of(&active);
        write_lease_file(self.pid, &desired, &loaded)
    }

    /// This guard's own tie-break priority for [`identifiers_i_should_lead`]
    /// (#2672): `(pid, token)`, a strict total order across every live
    /// guard on the machine — `pid` differs across processes (so a
    /// cross-process comparison always resolves on `pid` alone), and
    /// `token` (minted from the single process-wide [`NEXT_LEASE_TOKEN`]
    /// counter) already orders every guard WITHIN this one process
    /// consistently, so a same-pid comparison (same-process siblings)
    /// resolves on `token`.
    fn priority(&self) -> (u32, u64) {
        (self.pid, self.token)
    }

    /// Every `darkmux:*` model id leased by ANY other LIVE holder this
    /// guard should treat as pinned — other PROCESSES (via
    /// [`live_leased_models`]) UNIONED with every other SAME-PROCESS
    /// holder's own contribution to [`ACTIVE_LEASES`], excluding this
    /// guard's own token (#2663).
    ///
    /// [`live_leased_models`] alone is the gap #2663 closes: it excludes
    /// `own_pid` by construction (see its own doc — a process must never
    /// see its OWN lease as something to protect itself from), which is
    /// correct for a single-dispatch process but wrong the moment a
    /// SECOND concurrent in-process holder exists (`darkmux acp` running
    /// two overlapping `session/prompt` tasks, say). #2651 already made
    /// that second holder's contribution visible in the on-disk union —
    /// correct for an OTHER process's `live_leased_models(own_pid)` read —
    /// but nothing in THIS process ever consulted it, so a live
    /// same-process sibling could still be evicted mid-generation by a
    /// wave reconciling in the SAME process. This method is that missing
    /// same-process-inclusive read.
    ///
    /// Excludes `self.token` so a guard never sees ITS OWN just-written
    /// contribution as something to pin against itself — a guard's own
    /// desired placement is already accounted for by the caller's own
    /// planning input (the wave's own desired-set), never by being
    /// externally "pinned"; self-inclusion would make a wave unable to
    /// ever supersede its own prior placement when it legitimately needs
    /// to change what model it holds.
    ///
    /// A same-process holder that has actually DROPPED (its dispatch
    /// finished, withdrew, or panicked — `Drop` runs on every exit path,
    /// #2651) is no longer in [`ACTIVE_LEASES`] at all, so its models
    /// never appear here: withdrawal is real, never "pinned forever".
    pub fn all_live_leased_models(&self) -> Vec<String> {
        let mut models = live_leased_models(self.pid);
        let active = ACTIVE_LEASES.lock().unwrap_or_else(|poison| poison.into_inner());
        for (token, entry) in active.iter() {
            if *token == self.token {
                continue;
            }
            models.extend(entry.desired.iter().cloned());
        }
        drop(active);
        models.sort();
        models.dedup();
        models
    }

    /// (#2672 MUST FIX 1) Of `candidates` (this wave's own about-to-be-
    /// desired identifiers), which ones THIS guard should treat as NOT
    /// pinned despite appearing in [`all_live_leased_models`] — because
    /// every OTHER live holder naming that identifier is itself still only
    /// ACQUIRING it (`write`-ed as desired, never confirmed via
    /// [`mark_loaded`]), and this guard's own [`priority`] is the lowest
    /// among every such acquiring-only holder.
    ///
    /// The bug this closes: two concurrent same-process (or cross-process)
    /// waves that both merely INTEND to acquire the same identifier —
    /// neither one yet resident on it — each wrote their lease BEFORE
    /// planning (the existing #1487/#2651 discipline, unchanged), so each
    /// saw the OTHER as an already-claimed pin and both hit `Reason::
    /// ClaimedResidentInsufficientCtx`, burning their retry budget and
    /// failing where pre-#2669 both had simply succeeded. This method lets
    /// exactly ONE of them (the lowest-priority live holder — `priority`'s
    /// own doc: a strict total order, so never a tie) exclude the
    /// identifier and proceed with the real reconcile, while every OTHER
    /// racing guard keeps it pinned and takes the existing bounded
    /// retry-hold path in `ensure_wave_loaded`, which re-checks fresh
    /// residency on each attempt and converges to a plain `Reuse` once the
    /// leader's own reconcile has landed.
    ///
    /// An identifier ANY live holder has confirmed `loaded` (genuinely
    /// mid-generation, not merely intending to acquire) is NEVER returned
    /// here, regardless of priority — this is intent-vs-in-use, not
    /// "lower priority always wins": #2669's original protection against
    /// evicting a live, already-loaded sibling is completely untouched.
    pub fn identifiers_i_should_lead(&self, candidates: &[String]) -> Vec<String> {
        if candidates.is_empty() {
            return Vec::new();
        }
        let candidate_set: HashSet<&str> = candidates.iter().map(String::as_str).collect();
        let my_priority = self.priority();

        let mut in_use: HashSet<String> = HashSet::new();
        let mut acquiring_min: HashMap<String, (u32, u64)> = HashMap::new();

        {
            let active = ACTIVE_LEASES.lock().unwrap_or_else(|poison| poison.into_inner());
            for (token, entry) in active.iter() {
                if *token == self.token {
                    continue;
                }
                for m in &entry.loaded {
                    if candidate_set.contains(m.as_str()) {
                        in_use.insert(m.clone());
                    }
                }
                for m in &entry.desired {
                    if !candidate_set.contains(m.as_str()) || entry.loaded.contains(m) {
                        continue;
                    }
                    let p = (self.pid, *token);
                    acquiring_min
                        .entry(m.clone())
                        .and_modify(|existing| *existing = (*existing).min(p))
                        .or_insert(p);
                }
            }
        }

        for lease in live_foreign_leases(self.pid) {
            for m in &lease.loaded {
                if candidate_set.contains(m.as_str()) {
                    in_use.insert(m.clone());
                }
            }
            for m in &lease.models {
                if !candidate_set.contains(m.as_str()) || lease.loaded.contains(m) {
                    continue;
                }
                // No per-guard token crosses the process boundary (the
                // on-disk file already collapses a foreign process's own
                // same-process siblings into one union — see the module
                // doc) — `pid` alone is enough: it's never equal to our
                // own `self.pid` (a foreign lease is always some OTHER
                // live pid, by `live_foreign_leases`'s own construction),
                // so the tuple compare always resolves on the first
                // element for a cross-process pair.
                let p = (lease.pid, 0u64);
                acquiring_min
                    .entry(m.clone())
                    .and_modify(|existing| *existing = (*existing).min(p))
                    .or_insert(p);
            }
        }

        acquiring_min
            .into_iter()
            .filter(|(m, min_priority)| !in_use.contains(m) && my_priority < *min_priority)
            .map(|(m, _)| m)
            .collect()
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
            let (desired, loaded) = union_of(&active);
            let _ = write_lease_file(self.pid, &desired, &loaded);
        }
    }
}

/// Sorted, deduplicated union of every currently-registered holder's
/// `desired` set and, separately, `loaded` set — sorted so the on-disk file
/// (and every test asserting its content) is deterministic regardless of
/// token-insertion order or `HashMap` iteration order.
fn union_of(active: &HashMap<u64, LeaseEntry>) -> (Vec<String>, Vec<String>) {
    let mut desired: Vec<String> = active.values().flat_map(|e| e.desired.iter().cloned()).collect();
    desired.sort();
    desired.dedup();
    let mut loaded: Vec<String> = active.values().flat_map(|e| e.loaded.iter().cloned()).collect();
    loaded.sort();
    loaded.dedup();
    (desired, loaded)
}

/// Write `models`/`loaded` to `<pid>.lease`, wholesale — the shared private
/// I/O this module's writers ([`LeaseGuard::write`], [`LeaseGuard::
/// mark_loaded`], and `Drop`, all already holding [`ACTIVE_LEASES`]'s lock
/// when they call this) use, so the serialize-atomic-rename mechanics live
/// in exactly one place.
fn write_lease_file(pid: u32, models: &[String], loaded: &[String]) -> Result<()> {
    let dir = residency_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = lease_path(&dir, pid);
    let tmp = dir.join(format!("{pid}.lease.tmp"));
    let payload = LeaseFile { pid, models: models.to_vec(), loaded: loaded.to_vec() };
    let json = serde_json::to_string_pretty(&payload).context("serializing residency lease")?;
    fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("renaming residency lease into place at {}", path.display()))?;
    Ok(())
}

/// Every live-and-parseable lease file belonging to an OTHER pid — i.e.
/// every `<pid>.lease` file in the registry whose pid is (a) not `own_pid`
/// and (b) still alive — scanning `residency_dir()` once. The shared read
/// [`live_leased_models`] and [`LeaseGuard::identifiers_i_should_lead`]'s
/// cross-process half both build on, so the directory scan / pid-liveness
/// sweep / malformed-file leniency live in exactly one place. A lease
/// belonging to a dead pid is skipped from the result AND best-effort
/// removed (the crash-orphan sweep). A malformed or unreadable lease file
/// is skipped silently; this function never panics and never fails — a
/// registry read that can't be trusted degrades to "nothing pinned," which
/// is the same fail-open leniency `config.json` reads use elsewhere.
fn live_foreign_leases(own_pid: u32) -> Vec<LeaseFile> {
    let dir = residency_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
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
        out.push(lease);
    }
    out
}

/// Every `darkmux:*` model id leased (desired) by an OTHER live process —
/// see [`live_foreign_leases`] for the scan/liveness/leniency mechanics
/// this builds on.
pub fn live_leased_models(own_pid: u32) -> Vec<String> {
    let mut models = Vec::new();
    for lease in live_foreign_leases(own_pid) {
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
        let payload = LeaseFile { pid, models: vec!["darkmux:orphan".to_string()], loaded: vec![] };
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

    /// #2662 review finding: deleting `union.sort()` in `union_of` left all
    /// 9 pre-existing tests green — but only SOMETIMES. Eight repeats of
    /// `two_concurrent_in_process_holders_union_rather_than_clobber` under
    /// that mutant gave 5 passes and 3 failures, because that test's raw
    /// (pre-sort) flatten order depends on `HashMap`'s iteration order over
    /// `ACTIVE_LEASES`, which varies by process (a fresh random hasher seed
    /// per run) — for some seeds the two single-element contributions
    /// happen to land already in sorted order by luck, and a `dedup()`
    /// with no `sort()` first can't tell the difference.
    ///
    /// This test is constructed so that can never happen — it fails under
    /// the mutant on EVERY run, not just some, regardless of which of the
    /// `HashMap`'s many possible iteration orders occurs:
    ///
    /// 1. Guard 1's own contribution (`[b, zzdup, c]`) is internally out of
    ///    sorted order on its own (`"darkmux:zzdup"` > `"darkmux:c"`, yet
    ///    `zzdup` is written before `c`). Each guard's own `Vec` is
    ///    flattened as one contiguous block — `union_of` never reorders
    ///    WITHIN a single guard's contribution, only decides which
    ///    guard's block comes first — so that inversion survives into the
    ///    raw flatten no matter how `HashMap` orders the guards' blocks
    ///    relative to each other. A raw flatten containing `zzdup`
    ///    immediately before `c` can never equal the correctly sorted
    ///    vector (where every `c` precedes every `zzdup`), so the assertion
    ///    below is guaranteed to catch a missing `.sort()` on every run.
    /// 2. `"darkmux:zzdup"` is contributed by BOTH guard 1 and guard 2, and
    ///    in each case sandwiched strictly INTERIOR to that guard's own
    ///    block (never at a block's edge) — so its two occurrences can
    ///    never land adjacent to each other in the flatten regardless of
    ///    block order, meaning a mutant `dedup()` (which only ever
    ///    collapses ADJACENT duplicates) can never accidentally collapse
    ///    them by luck either. Real `dedup()` after a real `sort()`
    ///    collapses them unconditionally, so this also pins the "two
    ///    guards naming the same model" case named in review.
    #[serial_test::serial]
    #[test]
    fn sort_and_dedup_are_order_independent_across_concurrent_holders() {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::set(tmp.path());

        let guard1 = LeaseGuard::acquire();
        guard1
            .write(&["darkmux:b".to_string(), "darkmux:zzdup".to_string(), "darkmux:c".to_string()])
            .unwrap();

        let guard2 = LeaseGuard::acquire();
        guard2
            .write(&["darkmux:e".to_string(), "darkmux:zzdup".to_string(), "darkmux:f".to_string()])
            .unwrap();

        let guard3 = LeaseGuard::acquire();
        guard3.write(&["darkmux:a".to_string()]).unwrap();

        let guard4 = LeaseGuard::acquire();
        guard4.write(&["darkmux:g".to_string()]).unwrap();

        let other_own_pid = std::process::id().wrapping_add(1);
        let models = live_leased_models(other_own_pid);
        assert_eq!(
            models,
            vec![
                "darkmux:a".to_string(),
                "darkmux:b".to_string(),
                "darkmux:c".to_string(),
                "darkmux:e".to_string(),
                "darkmux:f".to_string(),
                "darkmux:g".to_string(),
                "darkmux:zzdup".to_string(),
            ],
            "the union across 4 concurrent same-process holders must be fully sorted and \
             deduplicated, regardless of HashMap iteration order over ACTIVE_LEASES"
        );

        drop(guard1);
        drop(guard2);
        drop(guard3);
        drop(guard4);
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

    // ── #2663: same-process-inclusive read ──────────────────────────────

    /// `all_live_leased_models` must union THREE sources into one pinned
    /// set — a genuinely live OTHER process's on-disk lease, a genuinely
    /// live SAME-PROCESS sibling guard's in-memory contribution — while
    /// excluding the CALLING guard's own just-written contribution, so it
    /// never pins its own about-to-be-superseded placement against
    /// itself.
    #[serial_test::serial]
    #[test]
    fn all_live_leased_models_unions_cross_process_and_same_process_siblings_excluding_self() {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::set(tmp.path());
        let dir = residency_dir();
        fs::create_dir_all(&dir).unwrap();

        // A genuinely live OTHER process's on-disk lease (a real spawned
        // child, not a hand-picked constant — same discipline the
        // cross-process tests above use).
        let mut holder = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawning a short-lived holder process");
        let holder_pid = holder.id();
        let cross_process_lease = LeaseFile {
            pid: holder_pid,
            models: vec!["darkmux:cross-process".to_string()],
            loaded: vec![],
        };
        fs::write(lease_path(&dir, holder_pid), serde_json::to_string(&cross_process_lease).unwrap())
            .expect("hand-writing a lease for the external holder pid");

        // A genuinely live SAME-PROCESS sibling.
        let sibling = LeaseGuard::acquire();
        sibling.write(&["darkmux:sibling".to_string()]).unwrap();

        // The CALLING guard's own contribution — must never appear in its
        // own result.
        let calling = LeaseGuard::acquire();
        calling.write(&["darkmux:calling-own".to_string()]).unwrap();

        let mut models = calling.all_live_leased_models();
        models.sort();
        assert_eq!(
            models,
            vec!["darkmux:cross-process".to_string(), "darkmux:sibling".to_string()],
            "must include the cross-process lease AND the same-process sibling's lease, while \
             excluding the calling guard's own just-written contribution: {models:?}"
        );

        let _ = holder.kill();
        let _ = holder.wait();
    }

    /// Dedup contract (#2666 CONSIDER 5): `all_live_leased_models`'s own
    /// doc + its `models.sort(); models.dedup();` tail promise a
    /// sorted-deduplicated result, but nothing exercised it — two DIFFERENT
    /// same-process siblings naming the SAME model id (a realistic shape:
    /// two overlapping dispatches both routed to the same catalog model)
    /// must collapse to one entry, not leak the duplicate into the
    /// operator-facing "holding {pinned:?}" string.
    #[serial_test::serial]
    #[test]
    fn all_live_leased_models_deduplicates_the_same_model_named_by_two_siblings() {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::set(tmp.path());

        // Two DIFFERENT same-process sibling guards, both naming the SAME
        // model id — the union before dedup would contain it twice.
        let sibling_a = LeaseGuard::acquire();
        sibling_a.write(&["darkmux:shared".to_string()]).unwrap();
        let sibling_b = LeaseGuard::acquire();
        sibling_b.write(&["darkmux:shared".to_string()]).unwrap();

        let calling = LeaseGuard::acquire();
        let models = calling.all_live_leased_models();

        assert_eq!(
            models,
            vec!["darkmux:shared".to_string()],
            "two siblings naming the same model id must collapse to ONE entry, sorted: {models:?}"
        );
    }

    /// Withdrawal (INVERTED direction, #2663): once a same-process
    /// sibling's guard has actually DROPPED, its model must no longer
    /// appear as pinned — a lease that never releases pins memory
    /// forever, the exact failure `residency_lease` exists to prevent.
    #[serial_test::serial]
    #[test]
    fn all_live_leased_models_stops_pinning_a_same_process_sibling_once_it_drops() {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::set(tmp.path());

        let calling = LeaseGuard::acquire();
        calling.write(&["darkmux:calling-own".to_string()]).unwrap();

        {
            let withdrawing = LeaseGuard::acquire();
            withdrawing.write(&["darkmux:withdrawing".to_string()]).unwrap();
            assert_eq!(
                calling.all_live_leased_models(),
                vec!["darkmux:withdrawing".to_string()],
                "while still held, the sibling's model is pinned"
            );
        } // `withdrawing` drops here.

        assert!(
            calling.all_live_leased_models().is_empty(),
            "once the sibling's guard drops, its model must no longer be pinned"
        );
    }

    /// Panic path (#2663): a same-process sibling that panics mid-dispatch
    /// still releases ONLY its own contribution via `Drop` during unwind
    /// (#2651's guarantee, exercised here through the new same-process-
    /// inclusive read) — a genuinely live survivor stays pinned, and the
    /// panicked holder's contribution is gone from the calling guard's
    /// own view once its `Drop` has run.
    #[serial_test::serial]
    #[test]
    fn all_live_leased_models_after_a_same_process_sibling_panics_keeps_the_survivor_drops_the_doomed(
    ) {
        let tmp = TempDir::new().unwrap();
        let _env = EnvGuard::set(tmp.path());

        let calling = LeaseGuard::acquire();
        calling.write(&["darkmux:calling-own".to_string()]).unwrap();

        let survivor = LeaseGuard::acquire();
        survivor.write(&["darkmux:survivor".to_string()]).unwrap();

        let unwound = std::panic::catch_unwind(|| {
            let doomed = LeaseGuard::acquire();
            doomed.write(&["darkmux:doomed".to_string()]).unwrap();
            panic!("simulated mid-dispatch panic (#2663 panic-path proof)");
        });
        assert!(unwound.is_err(), "precondition: the simulated panic must actually have unwound");

        assert_eq!(
            calling.all_live_leased_models(),
            vec!["darkmux:survivor".to_string()],
            "the survivor stays pinned; the panicked holder's contribution is gone (its Drop ran \
             during unwind) and the calling guard's own contribution is excluded"
        );

        drop(survivor);
    }
}
