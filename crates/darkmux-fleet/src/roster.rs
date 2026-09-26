//! Fleet roster — topology/roster data + reachability probes.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Default reachability-probe timeout. Matches `serve::PROBE_TIMEOUT_MS`
/// so a slow remote machine reads the same in `darkmux machine list` as
/// it does in the every-dispatch nudge.
const REACHABILITY_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// Default daemon port used when a ROSTER address omits an explicit
/// `:port`. Matches `darkmux_flow::daemon_probe::DEFAULT_DAEMON_PORT`'s
/// 8765.
///
/// (#2765) **Deliberately still a constant, and this is a boundary, not an
/// oversight.** #2765 made the LOCAL daemon's address configurable and
/// routed every client that looks for the LOCAL daemon through
/// `config_access::serve_port`/`serve_bind`. A roster entry is a different
/// thing: it names ANOTHER machine's daemon. Defaulting a peer's port to
/// this machine's `serve.port` would be right only for a homogeneous fleet
/// and silently wrong for a mixed one — and it would make one machine's
/// config quietly redirect traffic aimed at another, which is the class of
/// surprise #2765 exists to remove rather than relocate. The operator names
/// the port in the address when it differs (`machine add studio --address
/// 100.64.0.2:9000`), which is explicit input rather than a guess.
///
/// The same reasoning covers `fleet_cli::normalize_daemon_base` /
/// `fetch_machine_specs` and `darkmux_serve::peer_graph::
/// normalize_daemon_base`, which build peer base URLs from the same
/// portless form.
///
/// (#2782 C10, superseded by #2924) The one case this reasoning used not to
/// cover was a loopback SELF entry (`machine add <me> --address
/// 127.0.0.1:8765`, the old documented recipe), where the port was this
/// machine's. #2924 retired that recipe (`machine add` refuses loopback
/// unless `--allow-loopback`), and this machine's own entry is no longer
/// dialed at its roster address at all: `machine list`/`status`/`resources`
/// reach it at the local daemon (`serve_client_addr`). So this constant only
/// ever fills in the port of an address that names ANOTHER machine, or a
/// deliberate same-host `--allow-loopback` entry, which names its port.
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
/// fields this binary does not know are kept in `extras` and written back,
/// so a CLI verb run by an older binary does not drop a newer binary's field
/// (e.g. `loopback_intended`) or an operator's hand-added one (#2924 C-c).
/// Unknown fields at the roster's top level are NOT preserved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MachineEntry {
    /// Logical machine identifier — what flow records carry as
    /// `machine_id`. Operator-named (e.g. `"studio"`, `"laptop"`,
    /// `"mini-1"`). Unique within a roster.
    pub id: String,

    /// Tailnet DNS name to reach the daemon on. Examples: `"studio"`,
    /// `"studio.tailnet.ts.net"`, `"127.0.0.1:8765"`. Prefer the DNS
    /// name: a peer behind `tailscale serve` routes by Host header and
    /// will 404 a bare IP. A raw `host:port` works for a daemon bound
    /// directly to a non-loopback address. If no `:port` suffix is
    /// given, `DEFAULT_DAEMON_PORT` (8765) is assumed. Empty string is
    /// rejected at add time.
    pub address: String,

    /// Optional human-readable description for `fleet status` and the
    /// topology view tooltip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Unix-millis when this entry was added. Set on first add; preserved
    /// on subsequent edits. Used by `fleet status` to show fleet age.
    pub added_unix_ms: u64,

    /// Stable per-machine hardware identity (#2768) —
    /// `darkmux_hardware::machine_uid()`'s `IOPlatformUUID`, resolved at
    /// `machine add` time. This is the FLEET ROSTER's own copy of the same
    /// identity flow records carry as `machine_uid` (schema 1.11,
    /// `crates/darkmux-flow/src/schema.rs`) — the field that lets the
    /// viewer join a roster entry to the flow-derived card for the machine
    /// it actually describes, instead of the two rendering as separate
    /// cards the moment the operator's declared `id` and the machine's
    /// current `machine_id` diverge (a rename, a hostname change, three
    /// generations of both — the exact shape #2768 was filed from).
    ///
    /// `None` means *unknown identity*, never *equal to some name* — same
    /// rule the flow-record field's own doc states, and for the same
    /// reason: a consumer joining on this field must never fall back to
    /// comparing `id` strings when it is absent, which would reintroduce
    /// the unprovable-name guess the uid exists to replace. A `None` here
    /// is a legitimate, permanent state for many entries, not a gap to
    /// paper over — see `add_machine`'s own doc for exactly when a `None`
    /// stays a `None`.
    ///
    /// Absent on every roster entry saved before #2768. Lenient-on-read via
    /// `#[serde(default)]` (the roster's whole hand-edited-JSON contract,
    /// `FleetRoster`'s own doc) — an old entry loads exactly like a fresh
    /// one with no resolved identity, never an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_uid: Option<String>,

    /// (#2924) True when the operator added this entry with `machine add
    /// --allow-loopback`: a same-host test fleet, where a loopback address
    /// really does reach the peer. `darkmux doctor`'s `roster addresses` row
    /// reports such an entry as intentional instead of warning about it
    /// forever. Reset by any later `machine add` without the flag. Absent
    /// (false) on every entry written before #2924.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub loopback_intended: bool,

    /// (#2916 review C1) The overlay network's stable id for the node this
    /// entry's address reached, pinned by `machine add` or by the first
    /// token-bearing request from the CLI (a work submission, `machine
    /// status`/`resources <id>`, `machine list --deep`). EVERY token-bearing
    /// request (`darkmux_fleet::peer`) checks the node at the address is
    /// this one before anything is sent, and an entry with no pin must at
    /// least resolve to a tailnet node. The daemon's peer-graph proxy checks
    /// but never writes a pin. Cleared when `machine add` changes the
    /// address. Never printed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,

    /// Fields this binary does not know, preserved verbatim on rewrite.
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
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
    /// dropped `tier` field loads cleanly; since #2924 C-c the stale field is
    /// kept in the entry's `extras` and written back unchanged, like any
    /// other field this binary does not know. The tag is a
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
/// `env(DARKMUX_FLEET_FILE) > config.dirs.fleet_file > <darkmux root>/fleet.json`
/// (#2450: the root resolution honors `DARKMUX_HOME` and a project-local
/// `./.darkmux` before falling back to `~/.darkmux`). Delegates to the single
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
/// each running `darkmux machine add` on the same machine, or a CLI
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
        // Open-coded rather than routed through
        // `darkmux_types::flock::with_locked_file`'s closure wrapper
        // (#1352-adjacent cleanup): that helper releases the lock as
        // soon as the closure returns, which for a transaction ending
        // in a plain load→mutate→save would be equivalent timing here
        // too — but this call site's correctness has been reviewed
        // specifically for "lock held through the atomic rename," and
        // keeping the acquire/release explicit keeps that guarantee
        // visible at the call site rather than implicit in a shared
        // helper's contract. Still uses the ONE shared `FlockGuard`
        // type (`darkmux_types::flock`), so the RAII shape itself is
        // deduplicated — only the acquire/drop orchestration stays
        // local.
        let lock_path = path.with_extension("json.lock");
        let guard = darkmux_types::flock::lock_exclusive(&lock_path)?;

        let mut roster = load_roster()?;
        let result = f(&mut roster)?;
        save_roster(&roster)?;
        // Explicit drop after save so the lock isn't released until
        // the atomic rename has completed.
        drop(guard);
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
///
/// `uid` (#2768) — the machine's stable hardware identity, when the CALLER
/// was able to resolve one. There is exactly one case that can:
/// self-registration, where `cmd_machine_add` sees the entry's id equal this
/// machine's resolved machine_id (#2924; it used to key on a loopback
/// address) and reads `darkmux_hardware::machine_uid()` on the SAME host
/// this process is running on. A remote peer's hardware cannot be probed
/// from here — `machine add` performs no network call — so that caller
/// always passes `None`.
///
/// The merge rule mirrors `added_unix_ms`'s "don't clobber what a plain
/// re-add didn't recompute": a `Some` always WINS (self-registration
/// re-resolves fresh on every call — this host's own identity cannot go
/// stale between calls, so there is nothing to preserve over). A `None`
/// PRESERVES whatever this entry already carried, rather than erasing it:
/// a prior successful resolution, or an operator's hand-edit of the roster
/// JSON (a documented, supported way to set this file — `FleetRoster`'s
/// own doc). Without this preservation rule, updating a peer's
/// `--description` after hand-setting its `machine_uid` would silently
/// wipe the join back out, reintroducing the exact two-card defect #2768
/// exists to fix.
pub fn add_machine(
    roster: &mut FleetRoster,
    id: &str,
    address: &str,
    description: Option<&str>,
    uid: Option<&str>,
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
        .unwrap_or_else(|_| {
            // (#906) Pre-epoch / NTP-skewed clock: 0 doubles as a sentinel,
            // so warn rather than silently stamping a bogus added-at time.
            eprintln!("darkmux: system clock is before the Unix epoch — stamping added_unix_ms=0");
            0
        });
    let existing = roster.machines.get(id);
    let existing_added_at = existing.map(|m| m.added_unix_ms);
    let existing_uid = existing.and_then(|m| m.machine_uid.clone());
    let existing_extras = existing.map(|m| m.extras.clone()).unwrap_or_default();
    // A pin belongs to the address it was made for.
    let existing_node = existing.filter(|m| m.address == address).and_then(|m| m.node_id.clone());
    let entry = MachineEntry {
        id: id.to_string(),
        address: address.to_string(),
        description: description.map(String::from),
        added_unix_ms: existing_added_at.unwrap_or(now),
        machine_uid: uid.map(String::from).or(existing_uid),
        loopback_intended: false,
        node_id: existing_node,
        extras: existing_extras,
    };
    roster.machines.insert(id.to_string(), entry);
    Ok(())
}

/// (#2916) The roster KEY naming `name`: machine names are ASCII
/// case-insensitive, so an exact key wins, else the one key equal ignoring
/// case. Two keys differing only in case (a roster written before this
/// rule) are ambiguous: an error naming both, never a pick by map order.
pub fn find_machine_key(roster: &FleetRoster, name: &str) -> Result<Option<String>> {
    if roster.machines.contains_key(name) {
        return Ok(Some(name.to_string()));
    }
    let hits: Vec<&String> = roster.machines.keys().filter(|k| k.eq_ignore_ascii_case(name)).collect();
    match hits.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some((*one).clone())),
        many => Err(anyhow!(
            "the roster has several entries for `{name}` differing only in case ({}); machine names \
             are case-insensitive, so remove all but one with `darkmux machine remove <exact name>`",
            many.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// (#2916) The roster entry for `name`, case-insensitively (see
/// [`find_machine_key`]).
pub fn find_machine<'a>(roster: &'a FleetRoster, name: &str) -> Result<Option<&'a MachineEntry>> {
    Ok(find_machine_key(roster, name)?.and_then(|k| roster.machines.get(&k)))
}

/// (#2916 review C1) Resolve a roster address's host to its IP addresses,
/// bounded like every roster lookup. Empty when it does not resolve.
pub fn resolve_host_addrs(address: &str) -> Vec<std::net::IpAddr> {
    let Some(host) = address_host(address) else { return Vec::new() };
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return vec![ip.to_canonical()];
    }
    use std::net::ToSocketAddrs;
    let (tx, rx) = std::sync::mpsc::channel();
    let q = format!("{host}:0");
    let _ = std::thread::Builder::new().name("darkmux-dns-resolve".into()).spawn(move || {
        let r: Vec<std::net::IpAddr> =
            q.to_socket_addrs().map(|it| it.map(|a| a.ip().to_canonical()).collect()).unwrap_or_default();
        let _ = tx.send(r);
    });
    let mut ips = rx.recv_timeout(DNS_RESOLUTION_TIMEOUT).unwrap_or_default();
    ips.dedup();
    ips
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

/// True when `address`'s host portion is a bare, non-loopback IP literal
/// (v4 or v6), with or without an explicit `:port` suffix or a
/// `scheme://` prefix — as opposed to a DNS name. (#1849) A peer behind
/// `tailscale serve` routes by Host header and 404s a bare IP even though
/// the daemon is healthy; this helper lets a 404 handler attach that
/// context without resolving anything or touching the network itself.
///
/// Loopback (`127.0.0.1`, `::1`, …) is excluded on purpose: loopback
/// traffic never traverses `tailscale serve` — an entry at a loopback
/// address (a same-host test fleet's `--allow-loopback` entry, or an old
/// self entry from before #2924 retired that recipe) hits a local daemon
/// directly, so a 404 there is never that failure mode and the hint would
/// be actively wrong.
pub fn address_host_is_bare_ip(address: &str) -> bool {
    let trimmed = address.trim();
    let without_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let without_scheme = without_scheme.trim_end_matches('/');
    // A bracketed or bare v6 literal parses whole, before any host:port
    // split — a bare v6 literal like `fd7a::1234` contains colons that
    // would otherwise be misread as a port separator (same trick as
    // `parse_address` below).
    let unbracketed = without_scheme
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(without_scheme);
    // A trailing dot is the absolute-FQDN convention; an IP literal
    // written that way (copy-pasted from somewhere that appends one) still
    // parses as the same IP once it's stripped.
    let unbracketed = unbracketed.strip_suffix('.').unwrap_or(unbracketed);
    if let Ok(ip) = unbracketed.parse::<std::net::IpAddr>() {
        return !ip.is_loopback();
    }
    match without_scheme.rsplit_once(':') {
        Some((host, _port)) => {
            let host = host
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .unwrap_or(host);
            let host = host.strip_suffix('.').unwrap_or(host);
            host.parse::<std::net::IpAddr>()
                .map(|ip| !ip.is_loopback())
                .unwrap_or(false)
        }
        None => false,
    }
}

/// True when `address` reaches only the machine that reads it: its host is
/// a loopback literal (`127.0.0.0/8`, `::1`) or a `localhost` name, with or
/// without an explicit `:port` suffix or a `scheme://` prefix.
///
/// (#2924) This is the question the roster needs answered, because a roster
/// entry is read by OTHER machines (the daemon serves the roster to every
/// viewer on the tailnet, and #2916 routes work to it). Such an address in a
/// roster names whichever machine reads it, never the one the entry
/// describes. `machine add` refuses one (short of `--allow-loopback` for a
/// same-host test fleet) and `darkmux doctor`'s `roster addresses` row flags
/// one already written.
///
/// History: #2768 used this predicate to decide SELF-registration (the docs
/// then said `machine add <me> --address 127.0.0.1:8765`), and excluded
/// `localhost` because the recipe named the literal IP. #2924 moved "is this
/// entry me?" to the entry's id matching this machine's machine_id, so the
/// predicate is free to answer the reachability question fully.
///
/// Sibling implementation to `address_host_is_bare_ip` (this file already
/// carries one other near-duplicate host-parse, `parse_address`'s own
/// as-is/DNS branches, for the same reason: each predicate answers a
/// different question about the same string).
pub fn address_host_is_loopback(address: &str) -> bool {
    let trimmed = address.trim();
    let without_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let without_scheme = without_scheme.trim_end_matches('/');
    let unbracketed = without_scheme
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(without_scheme);
    let unbracketed = unbracketed.strip_suffix('.').unwrap_or(unbracketed);
    if let Ok(ip) = unbracketed.parse::<std::net::IpAddr>() {
        return ip_reaches_only_reader(ip);
    }
    let host_is_local = |host: &str| {
        let host = host.strip_suffix('.').unwrap_or(host);
        if short_ipv4_reaches_only_reader(host) {
            return true;
        }
        let host = host.to_ascii_lowercase();
        host == "localhost" || host.ends_with(".localhost")
    };
    match without_scheme.rsplit_once(':') {
        Some((host, _port)) => {
            let host = host
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .unwrap_or(host);
            let host = host.strip_suffix('.').unwrap_or(host);
            host.parse::<std::net::IpAddr>()
                .map(ip_reaches_only_reader)
                .unwrap_or_else(|_| host_is_local(host))
        }
        None => host_is_local(without_scheme),
    }
}

/// (#2924 C-6) An IP literal that a client on the reading machine would
/// reach itself through: loopback, the unspecified address (a bind
/// directive; dialing it reaches the local host), or either one written
/// v4-mapped in v6 form (`::ffff:127.0.0.1`).
fn ip_reaches_only_reader(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_unspecified(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback() || v4.is_unspecified())
        }
    }
}

/// (#2924 C-6) The shortened IPv4 forms the system resolver accepts without
/// DNS (`127.1`, `127.0.1`, `0`): all-numeric dotted parts, reaching only the
/// reading machine when the first part is 127 (two or more parts) or every
/// part is zero. Literal parsing only; no lookup.
fn short_ipv4_reaches_only_reader(host: &str) -> bool {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() > 4 || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    let Ok(nums) = parts.iter().map(|p| p.parse::<u32>()).collect::<Result<Vec<_>, _>>() else {
        return false;
    };
    (nums.len() >= 2 && nums[0] == 127) || nums.iter().all(|n| *n == 0)
}

/// (#2916) The HOST a roster address names, without scheme, port, brackets,
/// path or trailing dot. This is what a roster address MEANS to fleet work
/// submission: the machine's name on the network (#2924: its tailnet DNS
/// name). The viewer daemon's port (or the `https://` a `tailscale serve`
/// front puts on it) is not part of it; the work-submission listener is that
/// host on `fleet.listener.port`. `None` for an empty address.
pub fn address_host(address: &str) -> Option<String> {
    let trimmed = address.trim();
    let rest = trimmed.split_once("://").map(|(_, r)| r).unwrap_or(trimmed);
    let rest = rest.split('/').next().unwrap_or(rest);
    let host = if let Some(inner) = rest.strip_prefix('[') {
        inner.split(']').next().unwrap_or(inner)
    } else if rest.parse::<std::net::IpAddr>().is_ok() {
        // A bare v6 literal contains colons that are not a port separator.
        rest
    } else {
        rest.rsplit_once(':').map(|(h, _)| h).unwrap_or(rest)
    };
    let host = host.trim_end_matches('.');
    (!host.is_empty()).then(|| host.to_string())
}

/// Parse an `address` string into a `SocketAddr`. Accepts:
/// - bare IPs: `100.64.0.2` (port defaults to `DEFAULT_DAEMON_PORT`)
/// - host:port: `100.64.0.2:8765` or `studio.tailnet:9999`
/// - DNS names: `studio.tailnet` (resolved via std)
pub(crate) fn parse_address(address: &str) -> Result<std::net::SocketAddr> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("empty address"));
    }
    // (#swarm-7) A bare IP literal — v4 OR v6 — gets the default port
    // directly, before any DNS machinery. This is what fixes bare
    // compressed IPv6: `fd7a::1234` CONTAINS `:`, so the no-colon
    // append-default-port fallback below never fired for it, and the as-is
    // resolution can't read it as `host:port` — every tailnet/loopback
    // peer registered by its bare v6 address was permanently "unreachable"
    // in `machine list`. (Bonus: bare v4 now skips the DNS-resolver thread
    // it never needed.) A bracketed literal without a port (`[fd7a::1234]`)
    // is normalized the same way — brackets are v6's port-delimiter
    // syntax, meaningless without one.
    let unbracketed = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    if let Ok(ip) = unbracketed.parse::<std::net::IpAddr>() {
        return Ok(std::net::SocketAddr::new(ip, DEFAULT_DAEMON_PORT));
    }
    // Try as-is (covers `host:port`, `ip:port`, and `[v6]:port`).
    if let Some(a) = resolve_with_timeout(trimmed)? {
        return Ok(a);
    }
    // Fall back to default port if no `:` in the string (DNS names).
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

#[cfg(test)]
mod parse_address_tests {
    use super::*;

    /// (#swarm-7) Bare compressed IPv6 — the case that never resolved. It
    /// CONTAINS `:`, so the append-default-port fallback was skipped, and
    /// the as-is resolution can't read it as `host:port`: every peer
    /// registered by its bare v6 tailnet/loopback address showed as
    /// permanently unreachable in `machine list`. All IP-literal forms are
    /// pure (the bare-IP branch returns before any DNS), so these tests
    /// touch no resolver.
    #[test]
    fn bare_ipv6_gets_the_default_port() {
        let a = parse_address("fd7a:115c:a1e0::1234").unwrap();
        assert_eq!(a.ip().to_string(), "fd7a:115c:a1e0::1234");
        assert_eq!(a.port(), DEFAULT_DAEMON_PORT);
        // Loopback shorthand — the most compressed form of all.
        let l = parse_address("::1").unwrap();
        assert_eq!(l.port(), DEFAULT_DAEMON_PORT);
    }

    #[test]
    fn bracketed_ipv6_without_port_is_normalized() {
        // Brackets are v6's port-delimiter syntax; without a port they are
        // noise, not an error.
        let a = parse_address("[fd7a:115c:a1e0::1234]").unwrap();
        assert_eq!(a.port(), DEFAULT_DAEMON_PORT);
    }

    #[test]
    fn bracketed_ipv6_with_port_keeps_its_port() {
        let a = parse_address("[fd7a:115c:a1e0::1234]:9999").unwrap();
        assert_eq!(a.port(), 9999);
    }

    #[test]
    fn bare_ipv4_and_ipv4_with_port_still_work() {
        // Bare v4 previously resolved through the DNS thread; the bare-IP
        // branch answers it directly now. Same result, no resolver.
        let a = parse_address("100.64.0.2").unwrap();
        assert_eq!(a.port(), DEFAULT_DAEMON_PORT);
        let b = parse_address("100.64.0.2:8765").unwrap();
        assert_eq!(b.port(), 8765);
    }

    #[test]
    fn empty_address_errors() {
        assert!(parse_address("  ").is_err());
    }
}

#[cfg(test)]
mod address_host_is_bare_ip_tests {
    use super::*;

    #[test]
    fn bare_ipv4_is_a_bare_ip() {
        assert!(address_host_is_bare_ip("100.64.0.5"));
    }

    #[test]
    fn ipv4_with_port_is_a_bare_ip() {
        assert!(address_host_is_bare_ip("100.64.0.5:8765"));
    }

    #[test]
    fn bare_ipv6_is_a_bare_ip() {
        assert!(address_host_is_bare_ip("fd7a:115c:a1e0::1234"));
    }

    #[test]
    fn bracketed_ipv6_with_port_is_a_bare_ip() {
        assert!(address_host_is_bare_ip("[fd7a:115c:a1e0::1234]:9999"));
    }

    #[test]
    fn scheme_prefixed_ip_is_still_a_bare_ip() {
        assert!(address_host_is_bare_ip("http://100.64.0.5:8765"));
    }

    // Inverted case (#1849 red-prove requirement): a DNS name — the form
    // the corrected help text now recommends — must never read as a bare
    // IP, with or without a port or scheme.
    #[test]
    fn dns_name_is_not_a_bare_ip() {
        assert!(!address_host_is_bare_ip("studio"));
        assert!(!address_host_is_bare_ip("studio:8765"));
        assert!(!address_host_is_bare_ip("studio.tailnet.ts.net"));
        assert!(!address_host_is_bare_ip("studio.tailnet.ts.net:8765"));
        assert!(!address_host_is_bare_ip("http://studio.tailnet.ts.net:8765"));
    }

    // (#1849 MUST FIX 1, red-prove both directions) Loopback is a bare IP
    // literal by shape, but it never traverses `tailscale serve`, so a 404
    // from a loopback entry must never carry the tailscale-serve hint.
    #[test]
    fn loopback_ipv4_is_not_a_bare_ip() {
        assert!(!address_host_is_bare_ip("127.0.0.1"));
        assert!(!address_host_is_bare_ip("127.0.0.1:8765"));
        assert!(!address_host_is_bare_ip("http://127.0.0.1:8765"));
        // The whole 127.0.0.0/8 range is loopback, not just 127.0.0.1.
        assert!(!address_host_is_bare_ip("127.5.5.5:8765"));
    }

    #[test]
    fn loopback_ipv6_is_not_a_bare_ip() {
        assert!(!address_host_is_bare_ip("::1"));
        assert!(!address_host_is_bare_ip("[::1]:8765"));
    }

    // The inverse of the two tests above: a real (non-loopback) bare IP
    // must still get the hint after the loopback exclusion — the guard
    // narrows, it doesn't disable.
    #[test]
    fn non_loopback_bare_ip_is_still_a_bare_ip_after_the_loopback_exclusion() {
        assert!(address_host_is_bare_ip("100.64.0.5"));
        assert!(address_host_is_bare_ip("100.64.0.5:8765"));
        assert!(address_host_is_bare_ip("fd7a:115c:a1e0::1234"));
    }

    // (#1849 CONSIDER 9) A trailing dot is the absolute-FQDN convention;
    // an IP literal written that way still reads as the same IP.
    #[test]
    fn trailing_dot_ip_is_still_a_bare_ip() {
        assert!(address_host_is_bare_ip("100.64.0.2."));
        assert!(address_host_is_bare_ip("100.64.0.2.:8765"));
    }
}

#[cfg(test)]
mod address_host_is_loopback_tests {
    use super::*;

    // The shape the pre-#2924 self-registration recipe wrote, which
    // `machine add` now refuses.
    #[test]
    fn bare_loopback_v4_is_loopback() {
        assert!(address_host_is_loopback("127.0.0.1"));
        assert!(address_host_is_loopback("127.0.0.1:8765"));
        assert!(address_host_is_loopback("http://127.0.0.1:8765"));
        // The whole 127.0.0.0/8 range is loopback, not just 127.0.0.1.
        assert!(address_host_is_loopback("127.5.5.5:8765"));
    }

    #[test]
    fn loopback_v6_is_loopback() {
        assert!(address_host_is_loopback("::1"));
        assert!(address_host_is_loopback("[::1]:8765"));
    }

    // Inverted case (red-prove requirement): a real, non-loopback peer
    // address — the shape every OTHER `machine add` call uses — must never
    // read as loopback, with or without a port or scheme.
    #[test]
    fn non_loopback_bare_ip_is_not_loopback() {
        assert!(!address_host_is_loopback("100.64.0.5"));
        assert!(!address_host_is_loopback("100.64.0.5:8765"));
        assert!(!address_host_is_loopback("fd7a:115c:a1e0::1234"));
    }

    // An ordinary DNS name is not loopback.
    #[test]
    fn a_peer_dns_name_is_not_loopback() {
        assert!(!address_host_is_loopback("studio"));
        assert!(!address_host_is_loopback("studio.tailnet.ts.net:8765"));
        assert!(!address_host_is_loopback("mylocalhost.example"));
    }

    // (#2924) `localhost` names the reading machine exactly like 127.0.0.1
    // does, and this predicate now answers "does this address reach only the
    // machine that reads it?" (`machine add` refuses such an address; doctor
    // flags one). It used to exclude `localhost` because it also decided
    // self-registration, which is now decided by machine_id instead.
    #[test]
    fn localhost_names_are_loopback() {
        assert!(address_host_is_loopback("localhost"));
        assert!(address_host_is_loopback("localhost:8765"));
        assert!(address_host_is_loopback("http://LocalHost:8765/"));
        assert!(address_host_is_loopback("localhost."));
        assert!(address_host_is_loopback("studio.localhost:8765"));
    }

    // (#2924 C-6) Literal forms that reach only the reading machine: the
    // unspecified address (a bind directive, dialed as loopback), v4-mapped
    // v6 loopback, and the shortened `127.x` forms the resolver accepts.
    // Literal parsing only: no DNS lookup.
    #[test]
    fn unspecified_mapped_and_short_loopback_literals_are_loopback() {
        for a in [
            "0.0.0.0", "0.0.0.0:8765", "[::]:8765", "::",
            "::ffff:127.0.0.1", "[::ffff:127.0.0.1]:8765", "[::ffff:0.0.0.0]:8765",
            "127.1", "127.1:8765", "127.0.1", "http://127.1:8765/",
            "0", "0:8765",
        ] {
            assert!(address_host_is_loopback(a), "{a} must read as loopback");
        }
        for a in ["::ffff:100.64.0.2", "[::ffff:100.64.0.2]:8765", "128.1", "10.1", "1270.0.0.1", "127.example.com", "127", "0.1", "0.0.0.1:8765"] {
            assert!(!address_host_is_loopback(a), "{a} must not read as loopback");
        }
    }

    #[test]
    fn trailing_dot_loopback_ip_is_still_loopback() {
        assert!(address_host_is_loopback("127.0.0.1."));
        assert!(address_host_is_loopback("127.0.0.1.:8765"));
    }
}
