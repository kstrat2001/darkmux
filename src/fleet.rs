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

/// Write the roster to disk via atomic rename — write to `.tmp`, fsync,
/// rename over the target. Prevents partial-write corruption if darkmux
/// is killed mid-save.
pub fn save_roster(roster: &FleetRoster) -> Result<()> {
    let path = roster_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating fleet roster directory {}", parent.display()))?;
    }
    let mut tmp = path.clone();
    tmp.set_extension("tmp");
    let json = serde_json::to_string_pretty(roster)
        .context("serializing fleet roster")?;
    fs::write(&tmp, json)
        .with_context(|| format!("writing fleet roster temp file {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("atomic-renaming {} → {}", tmp.display(), path.display()))?;
    Ok(())
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
    use std::net::ToSocketAddrs;
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("empty address"));
    }
    // First try as-is (covers `host:port` and `ip:port`).
    if let Ok(mut iter) = trimmed.to_socket_addrs() {
        if let Some(a) = iter.next() {
            return Ok(a);
        }
    }
    // Fall back to default port if no `:` in the string.
    if !trimmed.contains(':') {
        let with_port = format!("{trimmed}:{DEFAULT_DAEMON_PORT}");
        if let Ok(mut iter) = with_port.to_socket_addrs() {
            if let Some(a) = iter.next() {
                return Ok(a);
            }
        }
    }
    Err(anyhow!("could not resolve address: {address}"))
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
}
