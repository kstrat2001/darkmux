//! Fleet roster — topology/roster data + reachability probes.

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
pub(crate) const DEFAULT_DAEMON_PORT: u16 = 8765;

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
    /// Schema-version tag. `"2"` after #590 dropped `MachineEntry.tier`.
    /// Bumped if the roster format ever changes shape.
    ///
    /// **Advisory, not code-gated.** `load_roster` neither reads nor validates
    /// this tag, and the roster is operator-owned hand-edited JSON without
    /// `deny_unknown_fields` — so a legacy `"1"` roster still carrying the
    /// dropped `tier` field loads cleanly, the stale field is silently
    /// absorbed, and it's gone on the next `save_roster`. The tag is a
    /// human-facing format marker, not an enforced compat boundary. (Contrast
    /// `WorkJob`'s `WORK_JOB_SCHEMA_VERSION`, which IS a hard wire break via
    /// `deny_unknown_fields` because it's an on-the-wire message from a
    /// possibly-buggy publisher; the roster is local operator state.) (#590)
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
        // paths see version = "2".
        Self {
            version: default_roster_version(),
            machines: BTreeMap::new(),
        }
    }
}

fn default_roster_version() -> String {
    "2".to_string()
}

/// Resolve the roster file path. Precedence (#661 Slice 3):
/// `env(DARKMUX_FLEET_FILE) > config.dirs.fleet_file > ~/.darkmux/fleet.json`
/// (with a `.darkmux/fleet.json` HOME-less fallback). Delegates to the single
/// resolver in `darkmux_types::config_access`. Tests bypass via the env override.
pub fn roster_path() -> PathBuf {
    darkmux_types::config_access::fleet_file()
}

/// Load the roster from disk, returning an empty roster when the file
/// doesn't exist (fresh-install case). Errors only when the file exists
/// but can't be parsed — those are operator-fixable typos in the JSON.
pub fn load_roster() -> Result<FleetRoster> {
    let path = roster_path();
    if !path.exists() {
        return Ok(FleetRoster::default());
    }
    let bytes =
        fs::read(&path).with_context(|| format!("reading fleet roster from {}", path.display()))?;
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
pub(crate) fn save_roster(roster: &FleetRoster) -> Result<()> {
    let path = roster_path();
    let parent = path.parent().map(|p| p.to_path_buf());
    if let Some(parent) = parent.as_ref() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating fleet roster directory {}", parent.display()))?;
    }
    let mut tmp = path.clone();
    tmp.set_extension("tmp");
    let json = serde_json::to_string_pretty(roster).context("serializing fleet roster")?;
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
        fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
    }
}

/// `fsync(2)` a directory so the rename(2) that just landed inside it
/// reaches stable storage. POSIX-only — on non-Unix this is a no-op
/// (NTFS journaling provides similar guarantees automatically).
fn fsync_dir(dir: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        let f =
            fs::File::open(dir).with_context(|| format!("open dir {} for fsync", dir.display()))?;
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
    address: &str,
    description: Option<&str>,
) -> Result<()> {
    if id.trim().is_empty() {
        return Err(anyhow!("machine id must be non-empty"));
    }
    if address.trim().is_empty() {
        return Err(anyhow!("machine address must be non-empty"));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let existing_added_at = roster.machines.get(id).map(|m| m.added_unix_ms);
    let entry = MachineEntry {
        id: id.to_string(),
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
pub(crate) fn parse_address(address: &str) -> Result<std::net::SocketAddr> {
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
pub(crate) fn resolve_with_timeout(input: &str) -> Result<Option<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    use std::sync::mpsc;
    let owned = input.to_string();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("darkmux-dns-resolve".to_string())
        .spawn(move || {
            let result: Result<Option<std::net::SocketAddr>, std::io::Error> =
                owned.to_socket_addrs().map(|mut iter| iter.next());
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
