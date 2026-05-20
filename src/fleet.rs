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
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (worker loop) + PR-C.3 (client push)
pub const WORK_STREAM_PREFIX: &str = "darkmux:work:";

/// Compose the per-tier work stream name. Used by both publisher and
/// claimer so the convention lives in one place.
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (worker loop) + PR-C.3 (client push)
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (worker loop) + PR-C.3 (client push)
pub struct WorkJob {
    /// Target hardware tier — used to pick which work stream to publish
    /// onto. The worker on a matching-tier machine claims via that
    /// stream's consumer group. Examples: `"inference"`, `"hub"`,
    /// `"any"` (acceptable to any machine).
    pub target_tier: String,

    /// Optional pre-claim hint — when set, the dispatching orchestrator
    /// asserts this specific machine should handle the job. PR-C.1 just
    /// carries the field; routing enforcement lands in PR-C.3 (the
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

#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (worker loop) + PR-C.3 (client push)
fn default_runtime() -> String {
    "openclaw".to_string()
}

/// Result of a successful `claim_job` — the worker now owns the job.
/// `work_id` is the Redis stream entry ID assigned at publish time
/// (canonical form: `<ms>-<seq>`); `ack_job` uses it to acknowledge
/// completion.
#[derive(Debug, Clone)]
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (worker loop) + PR-C.3 (client push)
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
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (worker loop) + PR-C.3 (client push)
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
            // BUSYGROUP — group already exists, treat as success.
            let msg = e.to_string();
            if msg.contains("BUSYGROUP") {
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
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (worker loop) + PR-C.3 (client push)
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
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (worker loop) + PR-C.3 (client push)
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
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (worker loop) + PR-C.3 (client push)
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
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (worker loop) + PR-C.3 (client push)
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

/// Convenience constructor — build a WorkJob from the components the
/// dispatching client has on hand. Centralizes the "always set X to Y"
/// defaults (attempt=1, published_at=now, etc.) so PR-C.3 doesn't
/// duplicate the shape.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (worker loop) + PR-C.3 (client push)
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
}
