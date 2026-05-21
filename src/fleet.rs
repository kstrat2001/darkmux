//! Fleet roster — the operator-declared set of machines that compose
//! a darkmux fleet (#162 Phase 5).
//!
//! Roster lives at `~/.darkmux/fleet.json` (override via `DARKMUX_FLEET_FILE`).
//! Operator hand-edits OK; `darkmux fleet add/remove` are convenience verbs
//! that mutate the JSON without losing operator-edited fields. Read by the
//! dispatch path (PR-C) and the topology view.
//!
//! Single-machine fleets work without any roster entries — `darkmux fleet
//! status` on a fresh install shows just the local machine and reports no
//! peers. Multi-machine fleets need `darkmux fleet add <id>` per peer (or
//! one-time bootstrap via `/darkmux-add-machine` skill, #176).
//!
//! Out of scope here (deferred to PR-C and beyond):
//! - work-queue publication (`darkmux:work:<tier>` streams)
//! - cross-machine work claim semantics + lease handling
//! - `darkmux fleet route` verb (orchestrator-explicit routing)
//!
//! See `/Users/kain/.../docs/...` and the design comment on #246 for the
//! full parallel-dispatch shape this composes into.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Default reachability-probe timeout. Matches `serve::PROBE_TIMEOUT_MS`
/// so a slow remote machine reads the same in `darkmux fleet status` as
/// it does in the every-dispatch nudge.
const REACHABILITY_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// Default daemon port used when an address omits an explicit `:port`.
/// Matches `serve::DEFAULT_DAEMON_ADDR`'s 8765.
const DEFAULT_DAEMON_PORT: u16 = 8765;

/// Hard cap on DNS resolution time inside `parse_address` (Wave-E.10
/// #255). `std::net::ToSocketAddrs::to_socket_addrs` blocks on the
/// system resolver with no timeout — a wedged DNS server can stall it
/// for seconds-to-minutes, making the 300ms `REACHABILITY_PROBE_TIMEOUT`
/// claim hollow. 2 seconds is generous for a healthy resolver
/// (typical lookup ≤ 50ms) and bounds the per-probe pre-flight cost
/// at a known ceiling. (PR-B review M-1)
const DNS_RESOLUTION_TIMEOUT: Duration = Duration::from_secs(2);

/// One machine in the fleet roster — operator-declared. Hand-edits OK;
/// CLI verbs preserve unknown fields via the BTreeMap shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MachineEntry {
    /// Logical machine identifier — what flow records carry as
    /// `machine_id`. Operator-named (e.g. `"studio"`, `"laptop"`,
    /// `"mini-1"`). Unique within a roster.
    pub id: String,

    /// Hardware tier this machine plays: `"inference"` (heavy-model
    /// peer), `"hub"` (always-on infra), `"client"` (UI-only). Future
    /// tier names pass through unchanged.
    pub tier: String,

    /// Tailnet address or DNS name to reach the daemon on. Examples:
    /// `"100.74.208.36"`, `"studio.tailnet"`, `"127.0.0.1:8765"`. If no
    /// `:port` suffix is given, `DEFAULT_DAEMON_PORT` (8765) is assumed.
    /// Empty string is rejected at add time.
    pub address: String,

    /// Optional human-readable description for `fleet status` and the
    /// topology view tooltip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Unix-millis when this entry was added. Set on first add; preserved
    /// on subsequent edits. Used by `fleet status` to show fleet age.
    pub added_unix_ms: u64,
}

/// The full roster — operator's declared fleet topology. Lives at
/// `~/.darkmux/fleet.json` by default; override via `DARKMUX_FLEET_FILE`.
///
/// JSON source-of-truth per CLAUDE.md doctrine — operators hand-edit the
/// file; the CLI just provides convenience verbs. Empty roster is the
/// default (fresh install) and is operator-correct for single-machine
/// fleets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetRoster {
    /// Schema-version tag. `"1"` for now. Bumped if the roster format
    /// ever changes shape.
    #[serde(default = "default_roster_version")]
    pub version: String,

    /// Machines keyed by id. BTreeMap so the on-disk JSON has stable key
    /// ordering across edits (operator diffs cleanly).
    #[serde(default)]
    pub machines: BTreeMap<String, MachineEntry>,
}

impl Default for FleetRoster {
    fn default() -> Self {
        // Hand-written so the in-memory default agrees with what a
        // freshly-deserialized `{}` would produce via serde — both
        // paths see version = "1".
        Self {
            version: default_roster_version(),
            machines: BTreeMap::new(),
        }
    }
}

fn default_roster_version() -> String {
    "1".to_string()
}

/// Resolve the roster file path: `DARKMUX_FLEET_FILE` env override, or
/// `~/.darkmux/fleet.json` default. Tests bypass via the env override.
pub fn roster_path() -> PathBuf {
    if let Ok(p) = std::env::var("DARKMUX_FLEET_FILE") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    dirs::home_dir()
        .map(|h| h.join(".darkmux").join("fleet.json"))
        .unwrap_or_else(|| PathBuf::from(".darkmux/fleet.json"))
}

/// Load the roster from disk, returning an empty roster when the file
/// doesn't exist (fresh-install case). Errors only when the file exists
/// but can't be parsed — those are operator-fixable typos in the JSON.
pub fn load_roster() -> Result<FleetRoster> {
    let path = roster_path();
    if !path.exists() {
        return Ok(FleetRoster::default());
    }
    let bytes = fs::read(&path)
        .with_context(|| format!("reading fleet roster from {}", path.display()))?;
    let roster: FleetRoster = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing fleet roster JSON at {}", path.display()))?;
    Ok(roster)
}

/// Run `f` against a freshly-loaded roster + persist the result, with
/// the whole load-modify-save cycle serialized by an exclusive
/// `flock(2)` on a sentinel file at `<roster_path>.lock`. Wave-E.12
/// (#255 / PR-B review): concurrent invocations (e.g. two operators
/// each running `darkmux fleet add` on the same machine, or a CLI
/// session racing with a background daemon update) previously dropped
/// entries to a last-writer-wins race — load, modify, save with no
/// serialization meant the second writer's `save_roster` clobbered the
/// first writer's add. With the flock guard, concurrent calls
/// serialize and every mutation is preserved.
///
/// The sentinel is a separate `.lock` file so the roster file itself
/// keeps the `0o600` mode set by `write_owner_only` (Wave-E.11) without
/// having to be opened+truncated by the lock acquisition.
///
/// POSIX-only — non-Unix falls through to a plain load-modify-save
/// (race window remains; tracked alongside the rest of the Windows
/// portability gaps).
pub fn mutate_roster<F, T>(f: F) -> Result<T>
where
    F: FnOnce(&mut FleetRoster) -> Result<T>,
{
    let path = roster_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating fleet roster directory {}", parent.display()))?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let lock_path = path.with_extension("json.lock");
        let lock_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening fleet lock file {}", lock_path.display()))?;
        let fd = lock_file.as_raw_fd();
        // Blocking exclusive lock — auto-released on file drop OR on
        // explicit LOCK_UN below (RAII guard).
        let lock_ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if lock_ret != 0 {
            return Err(anyhow!(
                "flock(LOCK_EX) failed on fleet lock {}: errno {}",
                lock_path.display(),
                std::io::Error::last_os_error()
            ));
        }
        struct FlockGuard(std::os::unix::io::RawFd);
        impl Drop for FlockGuard {
            fn drop(&mut self) {
                unsafe { libc::flock(self.0, libc::LOCK_UN) };
            }
        }
        let _guard = FlockGuard(fd);

        let mut roster = load_roster()?;
        let result = f(&mut roster)?;
        save_roster(&roster)?;
        // `lock_file` lives to here — explicit drop after save so the
        // lock isn't released until the rename has completed.
        drop(lock_file);
        Ok(result)
    }

    #[cfg(not(unix))]
    {
        let mut roster = load_roster()?;
        let result = f(&mut roster)?;
        save_roster(&roster)?;
        Ok(result)
    }
}

/// Write the roster to disk via durable atomic rename:
///   1. serialize roster to JSON
///   2. write to `<roster>.tmp` with mode `0o600` (Wave-E.11)
///   3. `fsync(2)` the tmp file so contents reach stable storage
///   4. `rename(2)` the tmp onto the target path (atomic on POSIX)
///   5. `fsync(2)` the parent directory so the rename itself reaches
///      stable storage
///
/// Steps 3 and 5 are the durability legs the doc comment previously
/// claimed but the code skipped (PR-B M-2 / Wave-E.13 #255). Without
/// them, a power failure between the rename and the next dirty-page
/// flush could leave the directory entry pointing at an old inode or
/// at no inode at all.
///
/// **Cross-process concurrency:** prefer `mutate_roster` for any
/// load-modify-save cycle. Direct `save_roster` callers must hold their
/// own external serialization (the existing call sites in tests do
/// because they're single-threaded). Without a flock-equivalent guard,
/// two parallel `save_roster` calls race on the shared `.tmp` path.
pub fn save_roster(roster: &FleetRoster) -> Result<()> {
    let path = roster_path();
    let parent = path.parent().map(|p| p.to_path_buf());
    if let Some(parent) = parent.as_ref() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating fleet roster directory {}", parent.display()))?;
    }
    let mut tmp = path.clone();
    tmp.set_extension("tmp");
    let json = serde_json::to_string_pretty(roster)
        .context("serializing fleet roster")?;
    write_owner_only(&tmp, json.as_bytes())
        .with_context(|| format!("writing fleet roster temp file {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("atomic-renaming {} → {}", tmp.display(), path.display()))?;
    if let Some(parent) = parent {
        fsync_dir(&parent)
            .with_context(|| format!("fsync parent directory {}", parent.display()))?;
    }
    Ok(())
}

/// Write `bytes` to `path` with mode `0o600` on POSIX (owner read/write
/// only) AND `fsync(2)` the file before the handle drops, so contents
/// reach stable storage before the caller's subsequent rename. Wave-E.11
/// added the mode; Wave-E.13 added the sync_all call (PR-B M-2).
fn write_owner_only(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("opening {} for owner-only write", path.display()))?;
        // `OpenOptions::mode` only applies on creation — explicitly
        // chmod for pre-existing files (e.g. the `.tmp` from a prior
        // crashed save).
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("setting 0o600 on {}", path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing bytes to {}", path.display()))?;
        // Force data + metadata to disk before the handle drops, so the
        // caller can rely on the file being durable before its rename.
        file.sync_all()
            .with_context(|| format!("fsync {}", path.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes)
            .with_context(|| format!("writing {}", path.display()))
    }
}

/// `fsync(2)` a directory so the rename(2) that just landed inside it
/// reaches stable storage. POSIX-only — on non-Unix this is a no-op
/// (NTFS journaling provides similar guarantees automatically).
fn fsync_dir(dir: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        let f = fs::File::open(dir)
            .with_context(|| format!("open dir {} for fsync", dir.display()))?;
        f.sync_all()
            .with_context(|| format!("sync_all on dir {}", dir.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Add or replace a machine entry in the roster. Idempotent — calling
/// twice with the same id updates the existing entry (preserving
/// `added_unix_ms`) rather than failing.
pub fn add_machine(
    roster: &mut FleetRoster,
    id: &str,
    tier: &str,
    address: &str,
    description: Option<&str>,
) -> Result<()> {
    if id.trim().is_empty() {
        return Err(anyhow!("machine id must be non-empty"));
    }
    if tier.trim().is_empty() {
        return Err(anyhow!("machine tier must be non-empty"));
    }
    if address.trim().is_empty() {
        return Err(anyhow!("machine address must be non-empty"));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let existing_added_at = roster
        .machines
        .get(id)
        .map(|m| m.added_unix_ms);
    let entry = MachineEntry {
        id: id.to_string(),
        tier: tier.to_string(),
        address: address.to_string(),
        description: description.map(String::from),
        added_unix_ms: existing_added_at.unwrap_or(now),
    };
    roster.machines.insert(id.to_string(), entry);
    Ok(())
}

/// Remove a machine from the roster. Returns the removed entry (so the
/// caller can confirm what was dropped) or `None` if not present.
pub fn remove_machine(roster: &mut FleetRoster, id: &str) -> Option<MachineEntry> {
    roster.machines.remove(id)
}

/// Reachability check via TCP connect to the daemon port. Same
/// short-budget shape as `serve::is_addr_reachable` — non-blocking;
/// degrades to "unreachable" on any error. Returns the elapsed probe
/// duration for diagnostic purposes (slow tailnet vs ECONNREFUSED).
pub fn probe_reachability(address: &str) -> ReachabilityResult {
    let parsed = parse_address(address);
    let socket_addr = match parsed {
        Ok(a) => a,
        Err(_) => {
            return ReachabilityResult {
                reachable: false,
                resolved_address: address.to_string(),
                elapsed_ms: 0,
                error: Some(format!("unparseable address: {address}")),
            };
        }
    };
    let start = std::time::Instant::now();
    let result = std::net::TcpStream::connect_timeout(&socket_addr, REACHABILITY_PROBE_TIMEOUT);
    let elapsed_ms = start.elapsed().as_millis() as u64;
    match result {
        Ok(_) => ReachabilityResult {
            reachable: true,
            resolved_address: socket_addr.to_string(),
            elapsed_ms,
            error: None,
        },
        Err(e) => ReachabilityResult {
            reachable: false,
            resolved_address: socket_addr.to_string(),
            elapsed_ms,
            error: Some(format!("{e}")),
        },
    }
}

/// Parse an `address` string into a `SocketAddr`. Accepts:
/// - bare IPs: `100.74.208.36` (port defaults to `DEFAULT_DAEMON_PORT`)
/// - host:port: `100.74.208.36:8765` or `studio.tailnet:9999`
/// - DNS names: `studio.tailnet` (resolved via std)
fn parse_address(address: &str) -> Result<std::net::SocketAddr> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("empty address"));
    }
    // First try as-is (covers `host:port` and `ip:port`).
    if let Some(a) = resolve_with_timeout(trimmed)? {
        return Ok(a);
    }
    // Fall back to default port if no `:` in the string.
    if !trimmed.contains(':') {
        let with_port = format!("{trimmed}:{DEFAULT_DAEMON_PORT}");
        if let Some(a) = resolve_with_timeout(&with_port)? {
            return Ok(a);
        }
    }
    Err(anyhow!("could not resolve address: {address}"))
}

/// Spawn `to_socket_addrs` in a thread + wait up to
/// `DNS_RESOLUTION_TIMEOUT` for the first socket address.
/// Returns:
/// - `Ok(Some(addr))` — resolution succeeded
/// - `Ok(None)` — resolution succeeded but returned no addresses
///   (caller distinguishes from "wrong format" — typically means the
///   host has no A/AAAA records of the right family)
/// - `Err` — DNS resolution timed out OR returned an error
///
/// Wave-E.10 #255: bounds the DNS leg of address parsing so
/// `probe_reachability`'s 300ms TCP budget claim isn't undermined
/// by an unbounded resolver lookup. Costs one thread spawn per
/// `parse_address` call — acceptable for the per-machine probe
/// frequency (≤ N per `fleet status`).
fn resolve_with_timeout(input: &str) -> Result<Option<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    use std::sync::mpsc;
    let owned = input.to_string();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("darkmux-dns-resolve".to_string())
        .spawn(move || {
            let result: Result<Option<std::net::SocketAddr>, std::io::Error> = owned
                .to_socket_addrs()
                .map(|mut iter| iter.next());
            // Ignore send errors — receiver may have already given up
            // on timeout. The thread cleans up on its own.
            let _ = tx.send(result);
        })
        .map_err(|e| anyhow!("spawning DNS-resolution thread: {e}"))?;

    match rx.recv_timeout(DNS_RESOLUTION_TIMEOUT) {
        Ok(Ok(Some(addr))) => {
            // Best-effort join — thread already done.
            let _ = handle.join();
            Ok(Some(addr))
        }
        Ok(Ok(None)) => {
            let _ = handle.join();
            Ok(None)
        }
        Ok(Err(e)) => {
            // to_socket_addrs surfaced a parse / NXDOMAIN error.
            let _ = handle.join();
            // For unparseable inputs (not a host:port shape) the OS
            // typically returns InvalidInput; treat as "no addresses"
            // so the caller's port-fallback path runs. For real
            // resolver errors (host not found), bubble up.
            if e.kind() == std::io::ErrorKind::InvalidInput {
                Ok(None)
            } else {
                Err(anyhow!("DNS resolution failed for {input}: {e}"))
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The thread will continue running until the OS resolver
            // returns; the parent has given up. This is the same
            // "leak the thread" tradeoff std uses everywhere for
            // unbounded I/O — better than blocking the caller forever.
            Err(anyhow!(
                "DNS resolution timed out for {input} after {}ms — \
                 check resolver health (`scutil --dns` on macOS, \
                 `resolvectl status` on systemd Linux)",
                DNS_RESOLUTION_TIMEOUT.as_millis()
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(anyhow!(
            "DNS resolution thread panicked or exited without sending result for {input}"
        )),
    }
}

/// Result of `probe_reachability` — surfaced in `fleet status` table.
#[derive(Debug, Clone, Serialize)]
pub struct ReachabilityResult {
    pub reachable: bool,
    pub resolved_address: String,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

// ─── Work queue (PR-C.1) ──────────────────────────────────────────────
//
// Per-tier Redis Streams that carry pending dispatch work for the fleet.
// One stream per tier — `darkmux:work:inference`, `darkmux:work:hub`,
// `darkmux:work:any` — so a worker on a tier-A machine only consumes
// jobs typed for its tier. The dispatching orchestrator publishes via
// `publish_job` (XADD); the daemon worker loop on the matching machine
// claims via `claim_job` (XREADGROUP BLOCK) and acks via `ack_job`
// (XACK) on completion. (#246 design, PR-C.1)
//
// **Pure substrate in this PR** — no client-side dispatching yet, no
// daemon worker loop yet. PR-C.2 wires the daemon worker; PR-C.3 wires
// the client-side push from `crew dispatch`. This file just gives both
// halves a shared vocabulary + a tested wire protocol.

/// Prefix for per-tier Redis Streams that carry work jobs.
/// Composed with a tier name to form the full stream key:
/// `darkmux:work:inference`, `darkmux:work:hub`, etc.
pub const WORK_STREAM_PREFIX: &str = "darkmux:work:";

/// Compose the per-tier work stream name. Used by both publisher and
/// claimer so the convention lives in one place.
pub fn work_stream_name(tier: &str) -> String {
    format!("{WORK_STREAM_PREFIX}{tier}")
}

/// MAXLEN cap for the work streams (approximate; passes `MAXLEN ~ N`
/// to XADD). 1000 in-flight + recently-acked jobs is generous — at
/// 2-machine fleet scale, the steady-state depth is bounded by
/// in-flight count (typically 1-2). The cap exists to prevent a stuck
/// or crashed worker from growing the stream unboundedly.
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (worker loop) + PR-C.3 (client push)
pub const WORK_STREAM_MAXLEN: usize = 1000;

/// One unit of dispatch work flowing through the queue. The producing
/// orchestrator constructs and publishes; the consuming worker reads,
/// dispatches, and acks. Serialized as the `record` field on a Redis
/// stream entry; the stream entry's auto-assigned ID becomes the
/// canonical `work_id` after claim.
///
/// Backward-compat shape: all fields are explicit (no `#[serde(default)]`
/// trickery) so any change to this struct is a deliberate schema bump.
/// Older worker code seeing a newer-shaped job will fail to deserialize
/// loudly rather than dispatching with missing context.
///
/// `#[serde(deny_unknown_fields)]` (PR-C.2) — a publisher cannot inject
/// extra fields that future-PR consumer code might inadvertently start
/// interpreting. Pairs with the schema-version contract; a real shape
/// change is a deliberate `WORK_JOB_SCHEMA_VERSION` bump + struct edit,
/// not a silent field smuggling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkJob {
    /// Target hardware tier — used to pick which work stream to publish
    /// onto. The worker on a matching-tier machine claims via that
    /// stream's consumer group. Examples: `"inference"`, `"hub"`,
    /// `"any"` (acceptable to any machine).
    pub target_tier: String,

    /// Optional pre-claim hint — when set, the dispatching orchestrator
    /// asserts this specific machine should handle the job. PR-C.1 just
    /// carries the field; routing enforcement lands in PR-C.2 (the
    /// claim path validates `target_machine` matches the local
    /// `DARKMUX_MACHINE_ID`). When `None`, the first matching-tier
    /// worker claims it (pull semantics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_machine: Option<String>,

    /// Role to dispatch against — `darkmux/<role-id>` resolves to the
    /// openclaw agent on the worker side.
    pub role_id: String,

    /// The operator's dispatch message — handed verbatim to the runtime.
    pub message: String,

    /// Stable session id used as the join key for `--wait` polling.
    /// Generated client-side; threaded to the dispatched `crew::dispatch::dispatch`
    /// via DispatchOpts so the emitted `dispatch.start` / `dispatch.complete`
    /// records carry the same value the publisher's poll loop is watching.
    pub session_id: String,

    /// Optional delivery target (`<channel>:<target>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver: Option<String>,

    /// Optional `--workdir` override (resolved to a string for transport;
    /// re-parsed to PathBuf on the worker side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,

    /// Optional sprint-id binding — same semantics as DispatchOpts.sprint_id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprint_id: Option<String>,

    /// Which runtime the worker should use — `"openclaw"` (default) or
    /// `"internal"`.
    #[serde(default = "default_runtime")]
    pub runtime: String,

    /// Per-turn timeout (seconds) — passes through to the runtime's
    /// turn timeout.
    pub timeout_seconds: u32,

    /// Unix-millis when the job was published. Used for queue-age
    /// diagnostics + the eureka rule that fires when total wall-clock
    /// < sum-of-sprint-wall-clocks (parallel-dispatch proof point).
    pub published_at_unix_ms: u64,

    /// Machine that published the job (the dispatching orchestrator's
    /// `DARKMUX_MACHINE_ID`). Read-only provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_by_machine: Option<String>,

    /// Orchestrator that published the job (the dispatching session's
    /// `DARKMUX_ORCHESTRATOR`). Read-only provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_by_orchestrator: Option<String>,

    /// Attempt counter — 1 on first publish, 2+ after a lease-expiry
    /// re-publish (PR-E semantics). PR-C.1 always publishes with 1.
    pub attempt: u32,
}

fn default_runtime() -> String {
    "openclaw".to_string()
}

/// Max byte size of a `WorkJob.message` accepted by the queue. A
/// publisher cannot XADD a multi-megabyte prompt that would force every
/// worker to allocate it on deserialize. 256 KiB matches the
/// reasoning-text cap in `dispatch_internal.rs` (#231 / S6) — same
/// rationale, same number. (#246 PR-C.2 boundary defense)
pub const MAX_WORK_MESSAGE_BYTES: usize = 256 * 1024;

/// Max byte size of `WorkJob.workdir` (the operator-supplied path
/// string). Filesystem path limits vary by platform; 4 KiB is generous
/// and prevents a publisher from filling memory with a multi-megabyte
/// path string. (#246 PR-C.2)
pub const MAX_WORK_WORKDIR_BYTES: usize = 4 * 1024;

/// Max length for identifier fields (`target_tier`, `target_machine`,
/// `role_id`). 64 chars is plenty for any realistic operator-named
/// machine or role id and forecloses identifier-as-payload attacks
/// (e.g. an `role_id` of 100MB). (#246 PR-C.2)
pub const MAX_WORK_IDENTIFIER_LEN: usize = 64;

/// Max allowed `timeout_seconds` on a queued `WorkJob`. 1 hour bounds
/// the worst-case "publisher pins this machine's single worker" surface.
/// Legitimate dispatches measured in this codebase top out around 15
/// minutes (long-agentic-shape workloads at large context); 1 hour is
/// 4× that headroom. A publisher specifying `u32::MAX` (136 years) is
/// rejected at the queue boundary. (#246 PR-C.3 / PR-C.2 review carry-over)
pub const MAX_WORK_TIMEOUT_SECONDS: u32 = 60 * 60;

impl WorkJob {
    /// Validate a `WorkJob` at the queue boundary — called by both the
    /// publisher (in `publish_job`) and the consumer (after claim, before
    /// dispatch). Enforces charset + size invariants that protect the
    /// downstream dispatch path from a hostile or buggy publisher.
    ///
    /// Validated:
    /// - Identifier fields (`target_tier`, optional `target_machine`,
    ///   `role_id`) match `[a-z0-9_-]{1,MAX_WORK_IDENTIFIER_LEN}`. Rejects
    ///   path-traversal (`../`), null bytes, command-injection chars,
    ///   and over-long values.
    /// - `runtime` is one of `"openclaw"` or `"internal"`. Future
    ///   runtime names require a deliberate code change here.
    /// - `message` ≤ `MAX_WORK_MESSAGE_BYTES`. Prevents memory
    ///   exhaustion at deserialize time.
    /// - Optional `workdir` ≤ `MAX_WORK_WORKDIR_BYTES`. The
    ///   symlink-escape check on the resolved path is done by the
    ///   worker (PR-C.2b / follow-up).
    pub fn validate(&self) -> Result<()> {
        validate_work_identifier("target_tier", &self.target_tier)?;
        if let Some(m) = &self.target_machine {
            validate_work_identifier("target_machine", m)?;
        }
        validate_work_identifier("role_id", &self.role_id)?;
        if !matches!(self.runtime.as_str(), "openclaw" | "internal") {
            return Err(anyhow!(
                "WorkJob.runtime must be 'openclaw' or 'internal' (got {:?})",
                self.runtime
            ));
        }
        if self.message.len() > MAX_WORK_MESSAGE_BYTES {
            return Err(anyhow!(
                "WorkJob.message exceeds {}-byte cap (was {} bytes)",
                MAX_WORK_MESSAGE_BYTES,
                self.message.len()
            ));
        }
        if let Some(w) = &self.workdir {
            if w.len() > MAX_WORK_WORKDIR_BYTES {
                return Err(anyhow!(
                    "WorkJob.workdir exceeds {}-byte cap (was {} bytes)",
                    MAX_WORK_WORKDIR_BYTES,
                    w.len()
                ));
            }
        }
        if self.timeout_seconds == 0 {
            return Err(anyhow!(
                "WorkJob.timeout_seconds must be non-zero (0 would never complete)"
            ));
        }
        if self.timeout_seconds > MAX_WORK_TIMEOUT_SECONDS {
            return Err(anyhow!(
                "WorkJob.timeout_seconds exceeds {}-second cap (was {})",
                MAX_WORK_TIMEOUT_SECONDS,
                self.timeout_seconds
            ));
        }
        Ok(())
    }
}

/// Charset+length check for an identifier-shaped field — the canonical
/// validator used both at the queue boundary (`WorkJob::validate`) and
/// at the CLI boundary (`darkmux mission dispatch <mission_id>` etc.,
/// Wave-E.5 #255).
///
/// Allowlist: `[a-z0-9_-]` (ASCII lowercase + digits + underscore +
/// hyphen), length 1..=MAX_WORK_IDENTIFIER_LEN. The full `label`
/// parameter lets callers name the offending field as the operator
/// thinks of it (`"mission_id"`, `"WorkJob.target_tier"`, etc.) so
/// errors are operator-actionable rather than internal-shape-leaky.
pub(crate) fn validate_identifier(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(anyhow!("{label} must be non-empty"));
    }
    if value.len() > MAX_WORK_IDENTIFIER_LEN {
        return Err(anyhow!(
            "{label} exceeds {}-char limit (was {} chars): {value:?}",
            MAX_WORK_IDENTIFIER_LEN,
            value.len()
        ));
    }
    let bad = value
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_'));
    if let Some(c) = bad {
        return Err(anyhow!(
            "{label} contains invalid char {c:?} (allowlist [a-z0-9_-]): {value:?}"
        ));
    }
    Ok(())
}

/// Wraps `validate_identifier` with the `"WorkJob.{field}"` label
/// prefix used throughout `WorkJob::validate`. Kept as a thin shim so
/// the existing internal call-sites read tightly.
fn validate_work_identifier(field: &str, value: &str) -> Result<()> {
    validate_identifier(&format!("WorkJob.{field}"), value)
}

/// Result of a successful `claim_job` — the worker now owns the job.
/// `work_id` is the Redis stream entry ID assigned at publish time
/// (canonical form: `<ms>-<seq>`); `ack_job` uses it to acknowledge
/// completion.
#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub work_id: String,
    pub job: WorkJob,
}

/// Publish a job onto the per-tier Redis Stream. Returns the
/// auto-assigned entry ID (the canonical `work_id`).
///
/// XADD fields:
/// - `schema`: `WORK_JOB_SCHEMA_VERSION` ("1") — wire-version tag so
///   future schema bumps can be detected by older workers
/// - `record`: the JSON-serialized WorkJob
///
/// Capped at `WORK_STREAM_MAXLEN` via `MAXLEN ~ N` so a stuck worker
/// can't grow the stream unboundedly.
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (worker loop) + PR-C.3 (client push)
pub fn publish_job(client: &redis::Client, job: &WorkJob) -> Result<String> {
    // Fail-fast at the queue boundary — better to reject a malformed
    // job at publish than to ship it across the network and trip the
    // consumer-side validator after one or more workers waste their
    // claim budget on it. (#246 PR-C.2)
    job.validate().context("validating WorkJob before publish")?;
    let mut conn = client
        .get_connection()
        .context("opening Redis connection to publish work job")?;
    let stream = work_stream_name(&job.target_tier);
    let payload =
        serde_json::to_string(job).context("serializing WorkJob")?;
    let mut cmd = redis::cmd("XADD");
    cmd.arg(&stream)
        .arg("MAXLEN")
        .arg("~")
        .arg(WORK_STREAM_MAXLEN)
        .arg("*")
        .arg("schema")
        .arg(WORK_JOB_SCHEMA_VERSION)
        .arg("record")
        .arg(&payload);
    let id: String = cmd
        .query(&mut conn)
        .with_context(|| format!("XADD to {stream}"))?;
    Ok(id)
}

/// Wire-schema version tag carried alongside each job. Bumped when
/// `WorkJob` shape changes in a way old workers can't safely parse.
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (worker loop) + PR-C.3 (client push)
pub const WORK_JOB_SCHEMA_VERSION: &str = "1";

/// Ensure the consumer group exists on the per-tier stream. Idempotent —
/// returns `Ok(())` whether the group was just created OR already
/// existed. The `MKSTREAM` flag creates the stream itself if missing
/// (XGROUP CREATE on a non-existent stream would otherwise error).
///
/// Call once per daemon-startup per tier. Safe to call repeatedly.
pub fn init_consumer_group(
    client: &redis::Client,
    tier: &str,
    group: &str,
) -> Result<()> {
    let mut conn = client
        .get_connection()
        .context("opening Redis connection to init consumer group")?;
    let stream = work_stream_name(tier);
    let result: redis::RedisResult<String> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(&stream)
        .arg(group)
        .arg("$")
        .arg("MKSTREAM")
        .query(&mut conn);
    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            // BUSYGROUP → group already exists; treat as success. Use the
            // typed error code (redis-rs 0.27+ `RedisError::code()`) rather
            // than substring-matching the Display string — survives future
            // crate-version reformatting of error messages.
            if matches!(e.code(), Some("BUSYGROUP")) {
                Ok(())
            } else {
                Err(anyhow!("XGROUP CREATE on {stream}: {e}"))
            }
        }
    }
}

/// Claim the next job from the per-tier stream's consumer group via
/// XREADGROUP. Blocks for up to `block_ms` waiting for a new entry;
/// returns `Ok(None)` on timeout (no work available).
///
/// Returns the entry ID (used for `ack_job`) plus the deserialized
/// `WorkJob`. Malformed entries (deserialize failure) are surfaced as
/// errors so the caller can decide whether to ack-and-skip or bail.
///
/// `consumer` is the per-worker identity (typically `DARKMUX_MACHINE_ID`)
/// — Redis tracks per-consumer pending-entries lists for lease semantics
/// (PR-E will consume these).
pub fn claim_job(
    client: &redis::Client,
    tier: &str,
    group: &str,
    consumer: &str,
    block_ms: u64,
) -> Result<Option<ClaimedJob>> {
    let mut conn = client
        .get_connection()
        .context("opening Redis connection to claim work job")?;
    let stream = work_stream_name(tier);

    // XREADGROUP returns nested arrays: [[stream, [[id, [k, v, k, v]]]]].
    // We parse via `redis::Value` (the dynamic type) rather than a typed
    // tuple to keep the parser robust across redis-rs versions.
    let raw: Option<redis::Value> = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg(group)
        .arg(consumer)
        .arg("COUNT")
        .arg(1usize)
        .arg("BLOCK")
        .arg(block_ms)
        .arg("STREAMS")
        .arg(&stream)
        .arg(">")
        .query(&mut conn)
        .with_context(|| format!("XREADGROUP from {stream}"))?;

    let Some(value) = raw else { return Ok(None) };
    let claimed = parse_xreadgroup_response(&value)?;
    Ok(claimed)
}

/// Parse XREADGROUP's nested-array response into an optional ClaimedJob.
/// Returns `Ok(None)` when the response is empty (timeout / no work);
/// extracted as a pure function so it's unit-testable without Redis.
fn parse_xreadgroup_response(value: &redis::Value) -> Result<Option<ClaimedJob>> {
    use redis::Value as V;

    // Bulk(nil) or Nil → timeout, no work.
    if matches!(value, V::Nil) {
        return Ok(None);
    }

    // Expected shape: Bulk([Bulk([stream_name, Bulk([Bulk([id, Bulk([k,v,k,v...])])])])])
    let outer = match value {
        V::Array(b) => b,
        _ => return Err(anyhow!("XREADGROUP: unexpected outer shape: {value:?}")),
    };
    if outer.is_empty() {
        return Ok(None);
    }
    let stream_block = match &outer[0] {
        V::Array(b) => b,
        _ => return Err(anyhow!("XREADGROUP: expected stream block")),
    };
    if stream_block.len() < 2 {
        return Ok(None);
    }
    let entries = match &stream_block[1] {
        V::Array(b) => b,
        _ => return Err(anyhow!("XREADGROUP: expected entries list")),
    };
    if entries.is_empty() {
        return Ok(None);
    }
    let entry = match &entries[0] {
        V::Array(b) => b,
        _ => return Err(anyhow!("XREADGROUP: expected entry tuple")),
    };
    if entry.len() < 2 {
        return Err(anyhow!("XREADGROUP: entry missing id or fields"));
    }
    let work_id = match &entry[0] {
        V::BulkString(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        V::SimpleString(s) => s.clone(),
        _ => return Err(anyhow!("XREADGROUP: expected entry id")),
    };
    let fields = match &entry[1] {
        V::Array(b) => b,
        _ => return Err(anyhow!("XREADGROUP: expected fields list")),
    };
    let record_json = extract_field(fields, "record")
        .ok_or_else(|| anyhow!("XREADGROUP entry missing `record` field"))?;
    let job: WorkJob = serde_json::from_str(&record_json)
        .with_context(|| format!("deserializing WorkJob from entry {work_id}"))?;
    Ok(Some(ClaimedJob { work_id, job }))
}

/// Pull a field's value out of a Redis field-list (`[k, v, k, v, ...]`).
/// Returns `None` if the key isn't present.
fn extract_field(fields: &[redis::Value], key: &str) -> Option<String> {
    use redis::Value as V;
    let mut i = 0;
    while i + 1 < fields.len() {
        let k = match &fields[i] {
            V::BulkString(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            V::SimpleString(s) => s.clone(),
            _ => {
                i += 2;
                continue;
            }
        };
        if k == key {
            return match &fields[i + 1] {
                V::BulkString(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
                V::SimpleString(s) => Some(s.clone()),
                _ => None,
            };
        }
        i += 2;
    }
    None
}

/// Acknowledge a claimed job, removing it from the consumer group's
/// pending-entries list (PEL). After ack, the job is fully delivered
/// from the queue's perspective.
///
/// Worker MUST call this after the dispatch completes, regardless of
/// dispatch success — the `dispatch.complete` flow record carries the
/// success/error signal; the ack just releases the queue lease.
pub fn ack_job(
    client: &redis::Client,
    tier: &str,
    group: &str,
    work_id: &str,
) -> Result<()> {
    let mut conn = client
        .get_connection()
        .context("opening Redis connection to ack work job")?;
    let stream = work_stream_name(tier);
    let _: i64 = redis::cmd("XACK")
        .arg(&stream)
        .arg(group)
        .arg(work_id)
        .query(&mut conn)
        .with_context(|| format!("XACK on {stream}"))?;
    Ok(())
}

// ─── Daemon worker loop (PR-C.2) ──────────────────────────────────────
//
// Runs on a dedicated `std::thread` (not a tokio task) inside the
// `darkmux serve` daemon. Polls `darkmux:work:<own-tier>` via XREADGROUP
// with a short BLOCK budget; on claim, invokes the existing synchronous
// `crew::dispatch::dispatch(opts)` and acks on completion. The dispatch
// path is unchanged — whether work arrives via local CLI invocation OR
// queue claim, it lands at the same entry point.
//
// **Why a dedicated thread, not a tokio task:** the redis crate (sync)
// + `crew::dispatch::dispatch` (shells out to docker / openclaw, blocks
// 5+ minutes) would saturate the tokio executor. The thread runs
// independently of the axum server's runtime.

/// Consumer group name used by all darkmux workers. Per-tier; combined
/// with the stream name, every worker for a given tier shares the
/// group → exactly-one-consumer-per-job delivery.
pub const WORKER_CONSUMER_GROUP: &str = "darkmux-workers";

/// XREADGROUP BLOCK budget per poll. 2 seconds is short enough that
/// shutdown latency is bounded (the worker rechecks the shutdown flag
/// every BLOCK round) and long enough that a quiet queue doesn't
/// hot-spin Redis. (#246 PR-C.2)
const WORKER_BLOCK_MS: u64 = 2_000;

/// Spawn the daemon worker thread. Returns the JoinHandle so callers
/// can monitor (typically the daemon never joins — the worker runs
/// for the daemon's lifetime and dies when the process exits).
///
/// Reads three env vars at spawn time:
/// - `DARKMUX_REDIS_URL` — required; absent → worker doesn't start
/// - `DARKMUX_MACHINE_TIER` — required; absent → worker doesn't start
/// - `DARKMUX_MACHINE_ID` — used as consumer name (per-machine identity)
///
/// When prerequisites are missing, logs to stderr and returns a thread
/// that exits immediately (caller still gets a JoinHandle). This keeps
/// the daemon usable as an observability node even without queue
/// participation — same posture as the existing single-machine-fleet
/// default in `fleet status`.
pub fn spawn_worker_thread() -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("darkmux-worker".to_string())
        .spawn(worker_main)
        .expect("spawn darkmux-worker thread")
}

/// Entry point for the worker thread. Reads env config, opens Redis,
/// initializes the consumer group, then loops on claim/dispatch/ack.
fn worker_main() {
    let Some(redis_url) = std::env::var("DARKMUX_REDIS_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        eprintln!(
            "darkmux-worker: DARKMUX_REDIS_URL not set — fleet work queue disabled. \
             Daemon continues as observability/serve node only."
        );
        return;
    };

    let Some(tier) = crate::flow::resolve_machine_tier() else {
        eprintln!(
            "darkmux-worker: DARKMUX_MACHINE_TIER not set — fleet work queue disabled. \
             Set DARKMUX_MACHINE_TIER=<inference|hub|client> to enable."
        );
        return;
    };

    let machine_id = crate::flow::resolve_machine_id()
        .unwrap_or_else(|| "unknown".to_string());

    let url = crate::flow::RawRedisUrl::new(redis_url);
    let client = match redis::Client::open(url.expose_for_probe()) {
        Ok(c) => c,
        Err(e) => {
            // `{e:#}` walks the anyhow context chain — single-level
            // `{e}` would hide the underlying redis-rs cause behind
            // our `.with_context` wrapper. Operator needs the full
            // chain to diagnose. (PR-C.2 review carry-over)
            eprintln!(
                "darkmux-worker: failed to open Redis client ({url}): {e:#}. \
                 Queue worker disabled."
            );
            return;
        }
    };

    if let Err(e) = init_consumer_group(&client, &tier, WORKER_CONSUMER_GROUP) {
        eprintln!(
            "darkmux-worker: init_consumer_group on darkmux:work:{tier} failed: {e:#}. \
             Queue worker disabled."
        );
        return;
    }

    eprintln!(
        "darkmux-worker: started — tier={tier} consumer={machine_id} \
         stream={} group={}",
        work_stream_name(&tier),
        WORKER_CONSUMER_GROUP
    );

    loop {
        match claim_job(&client, &tier, WORKER_CONSUMER_GROUP, &machine_id, WORKER_BLOCK_MS) {
            Ok(None) => {
                // BLOCK timeout — no work. Loop and re-block.
                continue;
            }
            Ok(Some(claimed)) => {
                handle_claimed_job(&client, &tier, claimed);
            }
            Err(e) => {
                eprintln!(
                    "darkmux-worker: claim_job failed ({e}); backing off 1s"
                );
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Validate, dispatch, and ack one claimed job. Errors are logged and
/// the job is acked anyway — the `dispatch.complete` flow record (or
/// its absence) is the operator-visible signal; the ack just releases
/// the queue lease.
fn handle_claimed_job(
    client: &redis::Client,
    tier: &str,
    claimed: ClaimedJob,
) {
    let ClaimedJob { work_id, job } = claimed;
    let session_id = job.session_id.clone();
    let role_id = job.role_id.clone();
    eprintln!(
        "darkmux-worker: claimed work_id={work_id} role={role_id} \
         session={session_id} target_machine={:?} attempt={}",
        job.target_machine, job.attempt
    );

    // Boundary validation — reject malformed jobs at the consumer too,
    // even though `publish_job` validated. Belt-and-braces against a
    // hostile publisher who bypassed our publish path.
    if let Err(e) = job.validate() {
        eprintln!(
            "darkmux-worker: REJECTED claimed job {work_id}: {e:#}. \
             Acking to release queue lease; dispatch NOT invoked."
        );
        let _ = ack_job(client, tier, WORKER_CONSUMER_GROUP, &work_id);
        return;
    }

    // Workdir symlink-escape guard via the shared validator (Wave-E.2 /
    // #255). The dispatch path itself ALSO validates, but doing it
    // here is the canonical "queue boundary" check — operator sees
    // the rejection in the worker's flow records via dispatch.error,
    // not buried deep in the internal/openclaw dispatch path.
    if let Some(workdir_str) = &job.workdir {
        let path = std::path::Path::new(workdir_str);
        if let Err(e) = crate::workdir::validate_workdir(path) {
            eprintln!(
                "darkmux-worker: REJECTED claimed job {work_id}: workdir validation failed: {e:#}. \
                 Acking to release queue lease; dispatch NOT invoked."
            );
            let _ = ack_job(client, tier, WORKER_CONSUMER_GROUP, &work_id);
            return;
        }
    }

    // Optional target_machine pre-claim hint: when set, the publisher
    // asserted this specific machine should handle the job. If it
    // doesn't match the local machine_id, log a warning but proceed —
    // the queue already gave us the claim, refusing would orphan the
    // job (PR-E will handle this properly via lease re-publish).
    let local_machine = crate::flow::resolve_machine_id();
    if let Some(target) = &job.target_machine {
        if local_machine.as_deref() != Some(target.as_str()) {
            eprintln!(
                "darkmux-worker: target_machine={target:?} doesn't match \
                 local machine_id={local_machine:?}; proceeding (queue \
                 already claimed; PR-E will add lease re-publish)."
            );
        }
    }

    // Convert + dispatch. The dispatch function is synchronous and may
    // block several minutes for long-agentic dispatches.
    let opts = job.into_dispatch_opts();
    let dispatch_result = crate::crew::dispatch::dispatch(opts);

    match dispatch_result {
        Ok(outcome) => {
            eprintln!(
                "darkmux-worker: dispatched work_id={work_id} → exit_code={} \
                 stdout_bytes={} stderr_bytes={}",
                outcome.exit_code,
                outcome.stdout.len(),
                outcome.stderr.len(),
            );
        }
        Err(e) => {
            eprintln!(
                "darkmux-worker: dispatch ERROR work_id={work_id}: {e:#}. \
                 Acking to release queue lease; dispatch.complete flow \
                 record carries the failure detail."
            );
        }
    }

    if let Err(e) = ack_job(client, tier, WORKER_CONSUMER_GROUP, &work_id) {
        eprintln!("darkmux-worker: XACK failed for {work_id}: {e:#}");
    }
}

impl WorkJob {
    /// Convert a claimed `WorkJob` into the `DispatchOpts` shape the
    /// `crew::dispatch::dispatch` entry point consumes. Centralizes the
    /// queue → in-process boundary so PR-C.3's client path can be checked
    /// against this shape for round-trip parity.
    pub fn into_dispatch_opts(self) -> crate::crew::dispatch::DispatchOpts {
        use crate::crew::dispatch::{DispatchOpts, Runtime};
        let runtime = Runtime::parse(&self.runtime).unwrap_or(Runtime::Openclaw);
        DispatchOpts {
            role_id: self.role_id,
            message: self.message,
            deliver: self.deliver,
            session_id: Some(self.session_id),
            timeout_seconds: self.timeout_seconds,
            skip_preflight: false,
            watch_paths: vec![],
            workdir: self.workdir.map(PathBuf::from),
            sprint_id: self.sprint_id,
            runtime,
            // Worker-side opts: never recurse into the queue (would
            // ping-pong jobs back to redis); always run local synchronous.
            machine: None,
            wait: true,
        }
    }
}

// ─── Client-side --wait wrapper (PR-C.3) ──────────────────────────────
//
// After `publish_job` returns, the dispatching client can either return
// immediately (fire-and-forget; the operator polls flow stream from
// elsewhere) OR block until the worker's `dispatch.complete` flow
// record lands for the matching `session_id`. The `--wait` wrapper
// implements the blocking form by **polling the Redis flow stream**
// (`darkmux:flow`) — NOT the local file, because in a cross-machine
// dispatch the completion record lands on the WORKER's local file,
// not the publisher's. The Redis stream is the only substrate both
// machines write to (via the shared TeeSink → RedisSink composition).
//
// This is the architectural pivot that makes cross-machine `--wait`
// actually work — a CRITICAL fix surfaced in PR-C.3 review where the
// initial local-file-polling implementation would always time out.

/// Poll interval for the `wait_for_completion` Redis polling. (#246 PR-C.3)
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Cap on XRANGE entries scanned per poll iteration. Matches the typical
/// Redis stream MAXLEN of 10000 (set via `DARKMUX_REDIS_MAXLEN`); covers
/// a full re-scan per poll without pagination. If the stream legitimately
/// exceeds this in a single poll window the caller will see a delayed
/// completion (corrects on the next iteration). (#246 PR-C.3)
const WAIT_XRANGE_COUNT: usize = 10000;

/// Result of `wait_for_completion`. Outcome is the dispatch's
/// `result_class` from the flow record's payload — typically `"ok"` or
/// `"error"` (see `crew::dispatch::dispatch` for the canonical values).
/// `wall_ms` is from the same payload.
#[derive(Debug, Clone)]
pub struct CompletionResult {
    pub session_id: String,
    pub result_class: String,
    pub wall_ms: Option<u64>,
    /// Raw payload JSON for downstream consumers that want richer
    /// fields (e.g. `exit_code`, `total_turns`, `result_class`).
    /// Currently surfaced via `--json` only (PR-D mission dispatch
    /// reads this for sprint-level aggregation).
    #[allow(dead_code)] // consumed by PR-D mission dispatch fan-out aggregator
    pub payload: Option<serde_json::Value>,
}

/// Block until a `dispatch.complete` flow record for `session_id` lands
/// in the Redis flow stream, or `timeout` elapses. Returns the
/// completion result on success; bails when the timeout fires (the job
/// may still be running on the remote worker — the operator can re-tail
/// via `darkmux flow tail --session <id>` to keep watching).
///
/// Polls the Redis stream (default `darkmux:flow`; override via
/// `DARKMUX_REDIS_STREAM`) every `WAIT_POLL_INTERVAL` (250ms). Each
/// poll runs `XRANGE - + COUNT 10000` and scans for an entry whose
/// `record` field matches both the target `session_id` AND a
/// `dispatch complete` action. The full-scan-per-poll trades CPU for
/// correctness — the stream is bounded by `DARKMUX_REDIS_MAXLEN`
/// (typically 10000), so the worst-case scan is bounded too. v1 cost
/// model is fine; PR-E may add last-id tracking for efficiency.
///
/// **Why poll Redis, not the local file:** in a cross-machine dispatch
/// the worker writes the `dispatch.complete` record to its OWN local
/// `~/.darkmux/flows/<day>.jsonl`, not the publisher's. The Redis
/// stream is the only substrate both machines write to (the shared
/// `darkmux:flow` stream via the TeeSink → RedisSink composition).
/// (CRITICAL fix from PR-C.3 review)
pub fn wait_for_completion(
    redis_url: &crate::flow::RawRedisUrl,
    session_id: &str,
    timeout: Duration,
) -> Result<CompletionResult> {
    let client = redis::Client::open(redis_url.expose_for_probe())
        .with_context(|| format!("opening Redis to wait for completion of {session_id}"))?;
    let mut conn = client
        .get_connection()
        .with_context(|| format!("connecting to Redis to wait for completion of {session_id}"))?;

    let stream = std::env::var("DARKMUX_REDIS_STREAM")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "darkmux:flow".to_string());

    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > timeout {
            return Err(anyhow!(
                "wait_for_completion: no dispatch.complete for session_id={session_id} \
                 within {}s in Redis stream {stream}. The job may still be running on the \
                 worker — tail `darkmux flow tail --session {session_id}` to keep watching.",
                timeout.as_secs()
            ));
        }

        // XRANGE darkmux:flow - + COUNT 10000 — full-scan each poll. The
        // stream is bounded (MAXLEN ~ 10000) so the scan is bounded too.
        let raw: redis::Value = redis::cmd("XRANGE")
            .arg(&stream)
            .arg("-")
            .arg("+")
            .arg("COUNT")
            .arg(WAIT_XRANGE_COUNT)
            .query(&mut conn)
            .with_context(|| format!("XRANGE on flow stream {stream}"))?;

        if let Some(result) = scan_flow_entries_for_completion(&raw, session_id)? {
            return Ok(result);
        }

        std::thread::sleep(WAIT_POLL_INTERVAL);
    }
}

/// Walk XRANGE's nested-array response, scanning each entry's `record`
/// field for a `dispatch.complete` event matching `session_id`. Returns
/// the first match's CompletionResult, or `None` if no entry matches.
/// Pure function; unit-testable independent of live Redis.
fn scan_flow_entries_for_completion(
    raw: &redis::Value,
    session_id: &str,
) -> Result<Option<CompletionResult>> {
    use redis::Value as V;
    // Expected shape: Array([Array([id, Array([k, v, k, v, ...])])])
    let entries = match raw {
        V::Array(a) => a,
        V::Nil => return Ok(None),
        other => return Err(anyhow!("XRANGE: unexpected outer shape: {other:?}")),
    };
    for entry in entries {
        let parts = match entry {
            V::Array(p) => p,
            _ => continue,
        };
        if parts.len() < 2 {
            continue;
        }
        let fields = match &parts[1] {
            V::Array(f) => f,
            _ => continue,
        };
        let Some(record_str) = extract_field(fields, "record") else {
            continue;
        };
        if let Some(result) = match_completion(&record_str, session_id) {
            return Ok(Some(result));
        }
    }
    Ok(None)
}

/// Parse one record JSON; return `Some(CompletionResult)` when it's a
/// dispatch-completion event for the target `session_id`. Pure function;
/// unit-testable without live Redis.
///
/// Canonical action shape is `"dispatch complete"` (space, NOT dot) —
/// that's what every production emit site uses today
/// (`crew::dispatch::dispatch` openclaw path + `dispatch_internal::dispatch`
/// internal-runtime path). The dotted form `"dispatch.complete"` is
/// accepted as forward-compat in case a future cleanup migrates the
/// emitters to match the dotted-per-action-type convention of
/// `dispatch.turn` / `dispatch.tool` / etc. (PR-C.3 review HIGH-2)
fn match_completion(line: &str, target_session_id: &str) -> Option<CompletionResult> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let action = value.get("action").and_then(|v| v.as_str())?;
    if action != "dispatch complete" && action != "dispatch.complete" {
        return None;
    }
    let session = value.get("session_id").and_then(|v| v.as_str())?;
    if session != target_session_id {
        return None;
    }
    let payload = value.get("payload").cloned();
    let result_class = payload
        .as_ref()
        .and_then(|p| p.get("result_class"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let wall_ms = payload
        .as_ref()
        .and_then(|p| p.get("wall_ms"))
        .and_then(|v| v.as_u64());
    Some(CompletionResult {
        session_id: target_session_id.to_string(),
        result_class,
        wall_ms,
        payload,
    })
}

/// Convenience constructor — build a WorkJob from the components the
/// dispatching client has on hand. Centralizes the "always set X to Y"
/// defaults (attempt=1, published_at=now, etc.) so PR-C.3 doesn't
/// duplicate the shape.
#[allow(clippy::too_many_arguments)]
pub fn build_work_job(
    target_tier: String,
    target_machine: Option<String>,
    role_id: String,
    message: String,
    session_id: String,
    deliver: Option<String>,
    workdir: Option<String>,
    sprint_id: Option<String>,
    runtime: String,
    timeout_seconds: u32,
    published_by_machine: Option<String>,
    published_by_orchestrator: Option<String>,
) -> WorkJob {
    let published_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    WorkJob {
        target_tier,
        target_machine,
        role_id,
        message,
        session_id,
        deliver,
        workdir,
        sprint_id,
        runtime,
        timeout_seconds,
        published_at_unix_ms,
        published_by_machine,
        published_by_orchestrator,
        attempt: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    fn with_roster_env<F: FnOnce(&PathBuf)>(f: F) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("fleet.json");
        let prev = std::env::var("DARKMUX_FLEET_FILE").ok();
        unsafe { std::env::set_var("DARKMUX_FLEET_FILE", &path); }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&path)));
        match prev {
            Some(v) => unsafe { std::env::set_var("DARKMUX_FLEET_FILE", v); },
            None => unsafe { std::env::remove_var("DARKMUX_FLEET_FILE"); },
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    #[serial]
    fn load_missing_returns_empty_roster() {
        with_roster_env(|_| {
            let r = load_roster().unwrap();
            assert!(r.machines.is_empty());
            assert_eq!(r.version, "1");
        });
    }

    #[test]
    #[serial]
    fn add_then_load_round_trips() {
        with_roster_env(|_| {
            let mut r = FleetRoster::default();
            add_machine(&mut r, "studio", "hub", "100.74.208.36", Some("always-on m1 max")).unwrap();
            save_roster(&r).unwrap();

            let loaded = load_roster().unwrap();
            assert_eq!(loaded.machines.len(), 1);
            let entry = loaded.machines.get("studio").unwrap();
            assert_eq!(entry.tier, "hub");
            assert_eq!(entry.address, "100.74.208.36");
            assert_eq!(entry.description.as_deref(), Some("always-on m1 max"));
            assert!(entry.added_unix_ms > 0);
        });
    }

    #[test]
    #[serial]
    fn add_preserves_added_ts_on_re_add() {
        // Idempotency: re-adding the same id mutates other fields but
        // preserves the original added_unix_ms. The roster's "fleet age"
        // signal stays honest.
        with_roster_env(|_| {
            let mut r = FleetRoster::default();
            add_machine(&mut r, "studio", "hub", "addr-1", None).unwrap();
            let first_added = r.machines.get("studio").unwrap().added_unix_ms;
            std::thread::sleep(std::time::Duration::from_millis(2));
            add_machine(&mut r, "studio", "hub", "addr-2", Some("updated desc")).unwrap();
            let entry = r.machines.get("studio").unwrap();
            assert_eq!(entry.added_unix_ms, first_added, "added_ts must be preserved");
            assert_eq!(entry.address, "addr-2", "address must update");
            assert_eq!(entry.description.as_deref(), Some("updated desc"));
        });
    }

    #[test]
    fn add_rejects_empty_id() {
        let mut r = FleetRoster::default();
        let err = add_machine(&mut r, "", "hub", "addr", None).unwrap_err();
        assert!(err.to_string().contains("id must be non-empty"));
    }

    #[test]
    fn add_rejects_empty_tier() {
        let mut r = FleetRoster::default();
        let err = add_machine(&mut r, "studio", "", "addr", None).unwrap_err();
        assert!(err.to_string().contains("tier must be non-empty"));
    }

    #[test]
    fn add_rejects_empty_address() {
        let mut r = FleetRoster::default();
        let err = add_machine(&mut r, "studio", "hub", "", None).unwrap_err();
        assert!(err.to_string().contains("address must be non-empty"));
    }

    #[test]
    fn remove_returns_entry_when_present() {
        let mut r = FleetRoster::default();
        add_machine(&mut r, "studio", "hub", "addr", None).unwrap();
        let removed = remove_machine(&mut r, "studio").expect("entry present");
        assert_eq!(removed.id, "studio");
        assert!(r.machines.is_empty());
    }

    #[test]
    fn remove_returns_none_when_absent() {
        let mut r = FleetRoster::default();
        assert!(remove_machine(&mut r, "ghost").is_none());
    }

    #[test]
    fn parse_address_handles_bare_ip() {
        // Bare IP gets DEFAULT_DAEMON_PORT appended.
        let a = parse_address("127.0.0.1").unwrap();
        assert_eq!(a.port(), DEFAULT_DAEMON_PORT);
    }

    #[test]
    fn parse_address_handles_ip_port() {
        let a = parse_address("127.0.0.1:9999").unwrap();
        assert_eq!(a.port(), 9999);
    }

    #[test]
    fn parse_address_rejects_empty() {
        assert!(parse_address("").is_err());
        assert!(parse_address("   ").is_err());
    }

    #[test]
    fn parse_address_returns_within_bounded_time_for_real_ip() {
        // Sanity for the Wave-E.10 DNS timeout wrapper: real IPs
        // resolve well under the 2s DNS_RESOLUTION_TIMEOUT cap and
        // certainly under 1s. Catches a regression where the
        // wrapper added ms-scale latency to the happy path.
        let start = std::time::Instant::now();
        let _ = parse_address("127.0.0.1:8765").expect("real IP resolves");
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "real-IP parse should be fast; took {elapsed:?}"
        );
    }

    #[test]
    fn parse_address_returns_bounded_for_invalid_format() {
        // A syntactically invalid input should bail fast (not wait the
        // full DNS_RESOLUTION_TIMEOUT). resolve_with_timeout converts
        // InvalidInput → Ok(None), so the caller's port-fallback path
        // runs; total bounded by 2 × DNS_RESOLUTION_TIMEOUT worst case.
        let start = std::time::Instant::now();
        let _ = parse_address("not::a::valid::addr");
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "invalid-format parse should not hang; took {elapsed:?}"
        );
    }

    #[test]
    fn parse_address_dns_timeout_is_bounded() {
        // Wave-E.10 invariant: even pathological-looking inputs
        // (e.g. a `.invalid` TLD per RFC 6761 — guaranteed NXDOMAIN)
        // must return within roughly DNS_RESOLUTION_TIMEOUT. The
        // resolver typically returns NXDOMAIN well under the cap;
        // this test asserts the WRAPPER bounds the worst case.
        let start = std::time::Instant::now();
        let _ = parse_address("definitely-not-a-real-hostname-12345.example.invalid");
        let elapsed = start.elapsed();
        // 2× DNS_RESOLUTION_TIMEOUT covers the host-then-host:port
        // double-attempt + scheduler jitter; still bounded.
        assert!(
            elapsed < std::time::Duration::from_secs(6),
            "DNS-failed parse should bounce within ~2 * DNS_RESOLUTION_TIMEOUT; took {elapsed:?}"
        );
    }

    #[test]
    fn probe_reachability_returns_true_for_listening_port() {
        // Bind a real listener on a free port; confirm probe sees it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let r = probe_reachability(&addr.to_string());
        assert!(r.reachable, "listening port must be reachable; got error: {:?}", r.error);
    }

    #[test]
    fn probe_reachability_returns_false_for_closed_port() {
        // Port 1 (tcpmux) is well-known and unbound on a normal system.
        let r = probe_reachability("127.0.0.1:1");
        assert!(!r.reachable);
        assert!(r.error.is_some());
    }

    #[test]
    fn probe_reachability_handles_unparseable() {
        let r = probe_reachability("not::a::valid::addr");
        assert!(!r.reachable);
        assert!(r.error.as_deref().unwrap().contains("unparseable"));
    }

    #[test]
    #[serial]
    fn save_roundtrip_preserves_pretty_json() {
        with_roster_env(|path| {
            let mut r = FleetRoster::default();
            add_machine(&mut r, "studio", "hub", "100.74.208.36:8765", None).unwrap();
            save_roster(&r).unwrap();
            let raw = std::fs::read_to_string(path).unwrap();
            // Pretty-print means newlines + indent — at least one newline.
            assert!(raw.contains('\n'), "expected pretty JSON; got: {raw}");
            assert!(raw.contains("\"studio\""));
        });
    }

    // ─── Work queue (PR-C.1) ──────────────────────────────────────────

    #[test]
    fn work_stream_name_composes_tier() {
        assert_eq!(work_stream_name("inference"), "darkmux:work:inference");
        assert_eq!(work_stream_name("hub"), "darkmux:work:hub");
        assert_eq!(work_stream_name("any"), "darkmux:work:any");
    }

    #[test]
    fn work_job_serde_round_trips() {
        let job = build_work_job(
            "inference".to_string(),
            Some("laptop".to_string()),
            "coder".to_string(),
            "implement the feature".to_string(),
            "session-2026-05-20-abc".to_string(),
            None,
            Some("/tmp/workspace".to_string()),
            None,
            "openclaw".to_string(),
            600,
            Some("studio".to_string()),
            Some("claude-opus-4-7".to_string()),
        );
        let json = serde_json::to_string(&job).unwrap();
        let parsed: WorkJob = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, job);
        assert_eq!(parsed.attempt, 1, "new jobs publish with attempt=1");
        assert!(parsed.published_at_unix_ms > 0);
    }

    #[test]
    fn work_job_omits_none_fields_from_serialized() {
        // None-valued optional fields must be omitted from the wire form
        // so older workers (future-proof case) don't trip on
        // unexpected null values.
        let job = build_work_job(
            "any".to_string(),
            None, // target_machine None
            "scribe".to_string(),
            "draft a note".to_string(),
            "s-1".to_string(),
            None, // deliver None
            None, // workdir None
            None, // sprint_id None
            "openclaw".to_string(),
            300,
            None, // published_by_machine None
            None, // published_by_orchestrator None
        );
        let json = serde_json::to_string(&job).unwrap();
        assert!(!json.contains("target_machine"), "None target_machine must be omitted: {json}");
        assert!(!json.contains("deliver"), "None deliver must be omitted: {json}");
        assert!(!json.contains("workdir"), "None workdir must be omitted: {json}");
        assert!(!json.contains("sprint_id"), "None sprint_id must be omitted: {json}");
        assert!(!json.contains("published_by_machine"), "None published_by_machine must be omitted: {json}");
        assert!(!json.contains("published_by_orchestrator"), "None published_by_orchestrator must be omitted: {json}");
    }

    #[test]
    fn work_job_default_runtime_is_openclaw() {
        let json = r#"{
            "target_tier": "any",
            "role_id": "scribe",
            "message": "hi",
            "session_id": "s-1",
            "timeout_seconds": 300,
            "published_at_unix_ms": 0,
            "attempt": 1
        }"#;
        let parsed: WorkJob = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.runtime, "openclaw");
    }

    #[test]
    fn parse_xreadgroup_handles_nil() {
        // Timeout / no work case — Redis returns Nil.
        let result = parse_xreadgroup_response(&redis::Value::Nil).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_xreadgroup_handles_empty_bulk() {
        // Some redis-rs versions return Bulk(vec![]) for empty.
        let result = parse_xreadgroup_response(&redis::Value::Array(vec![])).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_xreadgroup_extracts_entry() {
        // Build the nested-array shape XREADGROUP returns:
        // [[stream_name, [[id, [k, v, k, v]]]]]
        use redis::Value as V;
        let job = build_work_job(
            "inference".to_string(),
            None,
            "coder".to_string(),
            "do the thing".to_string(),
            "s-test".to_string(),
            None,
            None,
            None,
            "openclaw".to_string(),
            600,
            None,
            None,
        );
        let job_json = serde_json::to_string(&job).unwrap();
        let entry_id = "1716192000000-0";
        let response = V::Array(vec![V::Array(vec![
            V::BulkString(b"darkmux:work:inference".to_vec()),
            V::Array(vec![V::Array(vec![
                V::BulkString(entry_id.as_bytes().to_vec()),
                V::Array(vec![
                    V::BulkString(b"schema".to_vec()),
                    V::BulkString(b"1".to_vec()),
                    V::BulkString(b"record".to_vec()),
                    V::BulkString(job_json.as_bytes().to_vec()),
                ]),
            ])]),
        ])]);

        let claimed = parse_xreadgroup_response(&response).unwrap().unwrap();
        assert_eq!(claimed.work_id, entry_id);
        assert_eq!(claimed.job, job);
    }

    #[test]
    fn parse_xreadgroup_errors_on_missing_record_field() {
        use redis::Value as V;
        // Entry has fields but no `record` key — caller can't dispatch.
        let response = V::Array(vec![V::Array(vec![
            V::BulkString(b"darkmux:work:inference".to_vec()),
            V::Array(vec![V::Array(vec![
                V::BulkString(b"1716192000000-0".to_vec()),
                V::Array(vec![
                    V::BulkString(b"schema".to_vec()),
                    V::BulkString(b"1".to_vec()),
                    // record field absent
                ]),
            ])]),
        ])]);
        let err = parse_xreadgroup_response(&response).unwrap_err();
        assert!(err.to_string().contains("missing `record`"));
    }

    #[test]
    fn extract_field_finds_value_by_key() {
        use redis::Value as V;
        let fields = vec![
            V::BulkString(b"schema".to_vec()),
            V::BulkString(b"1".to_vec()),
            V::BulkString(b"record".to_vec()),
            V::BulkString(b"{\"k\":\"v\"}".to_vec()),
        ];
        assert_eq!(extract_field(&fields, "schema").as_deref(), Some("1"));
        assert_eq!(extract_field(&fields, "record").as_deref(), Some("{\"k\":\"v\"}"));
        assert_eq!(extract_field(&fields, "absent"), None);
    }

    #[test]
    fn extract_field_handles_status_values() {
        // Some redis-rs versions return Status (SimpleString) for short
        // ASCII values.
        use redis::Value as V;
        let fields = vec![
            V::SimpleString("schema".to_string()),
            V::SimpleString("1".to_string()),
        ];
        assert_eq!(extract_field(&fields, "schema").as_deref(), Some("1"));
    }

    // ─── WorkJob::validate() (PR-C.2 boundary defense) ────────────────

    fn good_job() -> WorkJob {
        build_work_job(
            "inference".to_string(),
            None,
            "coder".to_string(),
            "do a thing".to_string(),
            "s-1".to_string(),
            None,
            None,
            None,
            "openclaw".to_string(),
            600,
            None,
            None,
        )
    }

    #[test]
    fn validate_accepts_well_formed_job() {
        assert!(good_job().validate().is_ok());
    }

    #[test]
    fn validate_rejects_path_traversal_in_role_id() {
        let mut j = good_job();
        j.role_id = "../../etc/passwd".to_string();
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("invalid char") || err.contains("role_id"));
    }

    #[test]
    fn validate_rejects_uppercase_in_identifier() {
        let mut j = good_job();
        j.role_id = "Coder".to_string();
        assert!(j.validate().is_err());
    }

    #[test]
    fn validate_rejects_too_long_identifier() {
        let mut j = good_job();
        j.role_id = "a".repeat(MAX_WORK_IDENTIFIER_LEN + 1);
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("exceeds") && err.contains("role_id"));
    }

    #[test]
    fn validate_rejects_unknown_runtime() {
        let mut j = good_job();
        j.runtime = "nuclear".to_string();
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("runtime"));
    }

    #[test]
    fn validate_rejects_empty_runtime() {
        let mut j = good_job();
        j.runtime = "".to_string();
        assert!(j.validate().is_err());
    }

    #[test]
    fn validate_rejects_oversize_message() {
        let mut j = good_job();
        j.message = "x".repeat(MAX_WORK_MESSAGE_BYTES + 1);
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("message") && err.contains("exceeds"));
    }

    #[test]
    fn validate_accepts_message_at_cap() {
        let mut j = good_job();
        j.message = "x".repeat(MAX_WORK_MESSAGE_BYTES);
        assert!(j.validate().is_ok());
    }

    #[test]
    fn validate_rejects_oversize_workdir() {
        let mut j = good_job();
        j.workdir = Some("x".repeat(MAX_WORK_WORKDIR_BYTES + 1));
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("workdir") && err.contains("exceeds"));
    }

    #[test]
    fn validate_rejects_target_machine_with_special_chars() {
        let mut j = good_job();
        j.target_machine = Some("studio$rm-rf".to_string());
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("target_machine") || err.contains("invalid char"));
    }

    #[test]
    fn validate_accepts_target_machine_none() {
        let mut j = good_job();
        j.target_machine = None;
        assert!(j.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_timeout() {
        let mut j = good_job();
        j.timeout_seconds = 0;
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("timeout_seconds") && err.contains("non-zero"));
    }

    #[test]
    fn validate_rejects_oversize_timeout() {
        let mut j = good_job();
        j.timeout_seconds = MAX_WORK_TIMEOUT_SECONDS + 1;
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("timeout_seconds") && err.contains("exceeds"));
    }

    #[test]
    fn validate_accepts_max_timeout() {
        let mut j = good_job();
        j.timeout_seconds = MAX_WORK_TIMEOUT_SECONDS;
        assert!(j.validate().is_ok());
    }

    // ─── match_completion (PR-C.3 --wait wrapper) ─────────────────────

    #[test]
    fn match_completion_matches_canonical_action() {
        // Canonical form today is "dispatch complete" (space) — every
        // production emit site uses this. PR-C.3 review HIGH-2 caught
        // the labels swapped in an earlier draft of this file.
        let line = r#"{
            "action": "dispatch complete",
            "session_id": "sess-A",
            "payload": {"result_class": "ok", "wall_ms": 12345}
        }"#;
        let result = match_completion(line, "sess-A").expect("matches");
        assert_eq!(result.session_id, "sess-A");
        assert_eq!(result.result_class, "ok");
        assert_eq!(result.wall_ms, Some(12345));
    }

    #[test]
    fn match_completion_matches_dotted_action_forward_compat() {
        // Forward-compat for a future emitter migration to the dotted
        // convention used by `dispatch.turn` / `dispatch.tool` / etc.
        // No production emit-site uses this today.
        let line = r#"{
            "action": "dispatch.complete",
            "session_id": "sess-B",
            "payload": {"result_class": "error"}
        }"#;
        let result = match_completion(line, "sess-B").expect("matches");
        assert_eq!(result.result_class, "error");
        assert_eq!(result.wall_ms, None);
    }

    #[test]
    fn match_completion_rejects_unrelated_session() {
        let line = r#"{
            "action": "dispatch complete",
            "session_id": "sess-A",
            "payload": {"result_class": "ok"}
        }"#;
        assert!(match_completion(line, "sess-B").is_none());
    }

    #[test]
    fn match_completion_rejects_dispatch_start() {
        let line = r#"{
            "action": "dispatch.start",
            "session_id": "sess-A"
        }"#;
        assert!(match_completion(line, "sess-A").is_none());
    }

    #[test]
    fn match_completion_handles_missing_payload() {
        let line = r#"{
            "action": "dispatch complete",
            "session_id": "sess-A"
        }"#;
        let result = match_completion(line, "sess-A").expect("matches");
        assert_eq!(result.result_class, "unknown");
        assert_eq!(result.wall_ms, None);
    }

    #[test]
    fn match_completion_ignores_malformed_line() {
        assert!(match_completion("not json", "sess-A").is_none());
        assert!(match_completion("{}", "sess-A").is_none());
        assert!(match_completion(r#"{"action": "dispatch complete"}"#, "sess-A").is_none());
    }

    // ─── scan_flow_entries_for_completion (PR-C.3 Redis-poll path) ────

    #[test]
    fn scan_flow_entries_handles_empty_stream() {
        // Empty XRANGE response = no entries yet, return None (not an error).
        let resp = redis::Value::Array(vec![]);
        let result = scan_flow_entries_for_completion(&resp, "sess-X").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn scan_flow_entries_handles_nil() {
        // Nil response (some redis-rs versions) — same as empty.
        let result = scan_flow_entries_for_completion(&redis::Value::Nil, "sess-X").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn scan_flow_entries_finds_completion_for_session() {
        use redis::Value as V;
        let record = r#"{
            "action": "dispatch complete",
            "session_id": "sess-target",
            "payload": {"result_class": "ok", "wall_ms": 5000}
        }"#;
        // Mock XRANGE response: Array([Array([id, Array([k,v,k,v])])])
        let resp = V::Array(vec![V::Array(vec![
            V::BulkString(b"1716192000000-0".to_vec()),
            V::Array(vec![
                V::BulkString(b"schema".to_vec()),
                V::BulkString(b"1.8.0".to_vec()),
                V::BulkString(b"record".to_vec()),
                V::BulkString(record.as_bytes().to_vec()),
            ]),
        ])]);
        let result = scan_flow_entries_for_completion(&resp, "sess-target").unwrap();
        let c = result.expect("matches");
        assert_eq!(c.session_id, "sess-target");
        assert_eq!(c.result_class, "ok");
        assert_eq!(c.wall_ms, Some(5000));
    }

    #[test]
    fn scan_flow_entries_skips_non_matching_sessions() {
        use redis::Value as V;
        let record_a = r#"{"action":"dispatch complete","session_id":"sess-A","payload":{"result_class":"ok"}}"#;
        let record_b = r#"{"action":"dispatch start","session_id":"sess-target"}"#;
        let resp = V::Array(vec![
            V::Array(vec![
                V::BulkString(b"1-0".to_vec()),
                V::Array(vec![V::BulkString(b"record".to_vec()), V::BulkString(record_a.as_bytes().to_vec())]),
            ]),
            V::Array(vec![
                V::BulkString(b"2-0".to_vec()),
                V::Array(vec![V::BulkString(b"record".to_vec()), V::BulkString(record_b.as_bytes().to_vec())]),
            ]),
        ]);
        let result = scan_flow_entries_for_completion(&resp, "sess-target").unwrap();
        // No `dispatch complete` for sess-target → None
        assert!(result.is_none());
    }

    // ─── #[serde(deny_unknown_fields)] (PR-C.2) ───────────────────────

    #[test]
    fn workjob_deserialize_rejects_unknown_field() {
        // A future-PR field smuggled by a malicious publisher must fail
        // to deserialize, not silently roundtrip.
        let json = r#"{
            "target_tier": "inference",
            "role_id": "coder",
            "message": "hi",
            "session_id": "s-1",
            "runtime": "openclaw",
            "timeout_seconds": 300,
            "published_at_unix_ms": 0,
            "attempt": 1,
            "future_priority_field": 999
        }"#;
        let result: Result<WorkJob, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "deny_unknown_fields must reject smuggled field; got: {:?}",
            result
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("future_priority_field") || err.contains("unknown field"),
            "error should name the unknown field: {err}"
        );
    }

    #[test]
    fn workjob_deserialize_accepts_known_fields_only() {
        // Sanity: the strict shape still accepts a valid job.
        let json = r#"{
            "target_tier": "inference",
            "role_id": "coder",
            "message": "hi",
            "session_id": "s-1",
            "runtime": "openclaw",
            "timeout_seconds": 300,
            "published_at_unix_ms": 0,
            "attempt": 1
        }"#;
        let parsed: WorkJob = serde_json::from_str(json).expect("valid job parses");
        assert_eq!(parsed.target_tier, "inference");
    }
}
