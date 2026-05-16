//! Flow observability — structured JSONL records for darkmux run tracking.
//!
//! # Storage model
//!
//! Records are appended to a per-day JSONL file (`YYYY-MM-DD.jsonl`) under
//! `~/.darkmux/flows/` (overridable via `DARKMUX_FLOWS_DIR`). The first write
//! atomically prepends a schema header so partial-file recovery is possible.

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const FLOW_SCHEMA_VERSION: &str = "1.3.0";

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Work,
    Machinery,
    Audit,
    Review,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Operator,
    Frontier,
    Local,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    Scope,
    Estimate,
    Dispatch,
    Review,
    Ship,
    Retrospect,
    /// Tier-decision record (#136): the frontier orchestrator's reasoning
    /// for routing this piece of work to local vs. holding in frontier.
    /// Emitted via `darkmux flow tier-decision`. Category typically
    /// `audit`; the `reasoning` field carries the operator-visible
    /// rationale. Serialized as `"tier-decision"`.
    TierDecision,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FlowRecord {
    pub ts: String,
    pub level: Level,
    pub category: Category,
    pub tier: Tier,
    pub stage: Stage,
    pub action: String,
    pub handle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sprint_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// LMStudio model id that handled this work, when known. Set on
    /// dispatch records (`tier=local, stage=dispatch`) so the viewer
    /// can render which model ran the work without cross-referencing
    /// the model-status pill's timestamp. Resolved from openclaw config
    /// at dispatch entry: first checks `agents.list[<agent-id>].model`,
    /// then falls back to `agents.defaults.model.primary` when absent.
    /// None for non-dispatch records (lifecycle transitions, sprint
    /// review verdicts) and for dispatches where the openclaw config
    /// can't be resolved. Schema 1.2 addition (#106).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Operator-facing reasoning for this record. Used primarily by
    /// tier-decision records (#136) where the frontier orchestrator
    /// explains WHY work was routed to local vs. held in frontier. The
    /// audit substrate's "why" layer. Schema 1.3 addition.
    ///
    /// Non-tier-decision records typically leave this `None`. When set
    /// on any record, it's free-form prose intended for human review
    /// (compliance audit, post-mortem, retrospective).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Parent mission id. Optional because some flow records aren't
    /// scoped to a mission (operator-initiated dispatches without an
    /// active mission, machinery events). Schema 1.3 addition (#136).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
}

/// Resolve the flows directory from env override (`DARKMUX_FLOWS_DIR`) or
/// default (`~/.darkmux/flows/`). Falls back to `/tmp/darkmux/flows/` if
/// neither is resolvable (CI / sandboxed environments without HOME).
pub(crate) fn flows_dir() -> PathBuf {
    std::env::var("DARKMUX_FLOWS_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".darkmux").join("flows")))
        .unwrap_or_else(|| PathBuf::from("/tmp/darkmux/flows"))
}

/// ISO 8601 UTC date string from current time — `YYYY-MM-DD`. Used for
/// per-day file naming (one JSONL file per UTC day), NOT for record `ts`.
pub(crate) fn day_utc_now() -> String {
    let secs = current_epoch_secs();
    let (y, m, d) = epoch_to_yyyymmdd(secs);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// ISO 8601 UTC datetime string from current time — `YYYY-MM-DDTHH:MM:SSZ`.
/// Used for `FlowRecord.ts`. Seconds precision is sufficient for the
/// dispatch / sprint timing surfaces; finer precision is a future bump.
pub(crate) fn ts_utc_now() -> String {
    let secs = current_epoch_secs();
    let (y, mo, d) = epoch_to_yyyymmdd(secs);
    let (h, mi, s) = epoch_to_hhmmss(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}

fn current_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Convert unix epoch seconds to (year, month, day) in UTC.
/// Civil calendar algorithm from Howard Hinnant (public-domain).
fn epoch_to_yyyymmdd(epochs: i64) -> (i32, u8, u8) {
    let days = epochs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp as i32 + 3 } else { mp as i32 - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u8, d as u8)
}

/// Convert unix epoch seconds to (hour, minute, second) in UTC.
fn epoch_to_hhmmss(epochs: i64) -> (u8, u8, u8) {
    let secs_of_day = epochs.rem_euclid(86_400);
    let h = (secs_of_day / 3600) as u8;
    let mi = ((secs_of_day % 3600) / 60) as u8;
    let s = (secs_of_day % 60) as u8;
    (h, mi, s)
}

// ─── FlowSink abstraction (#162 Phase 1) ─────────────────────────────────
//
// `FlowSink` is the trait every flow record is written through. The current
// (and default) implementation is `LocalFileSink` — preserves the existing
// per-day JSONL behavior. Future implementations (Phase 3+) include
// `RedisSink` (XADD to a Redis Stream for fleet coordination) and `TeeSink`
// (write to multiple sinks during migration). See [#162] for the full arc.
//
// Per-process default sink: `default_sink()` returns the singleton sink the
// public `record()` dispatches through. Tests can override via
// `set_default_sink_for_tests`.

/// Abstraction over the destination of a flow record. Implementations
/// own the persistence semantics for their backend (file append, network
/// publish, etc.). All implementations must be `Send + Sync` because the
/// default sink is a process-wide singleton accessed from multiple
/// dispatch paths.
pub trait FlowSink: Send + Sync {
    /// Write a single record. Returns `Err` on persistence failure; the
    /// caller decides whether to bail or proceed (most current callers
    /// use `let _ = flow::record(...)` because audit-log writes are
    /// best-effort, but the trait signature is fallible for callers
    /// that DO want to react to write failures — e.g., a fleet
    /// coordinator might want to fall back to a local-file sink on
    /// network failure).
    fn write(&self, record: &FlowRecord) -> Result<()>;
}

/// File-based flow sink: appends to per-day JSONL files under
/// `~/.darkmux/flows/YYYY-MM-DD.jsonl`. The implementation darkmux has
/// shipped since v1.0 of the flow schema; preserved verbatim under
/// the trait abstraction.
///
/// Resolves the flows directory via `flows_dir()` at write time, NOT at
/// construction — so tests + operators that override `DARKMUX_FLOWS_DIR`
/// don't need to rebuild the sink to pick up the change. Symmetric with
/// how `record_at()` behaves today; refactor preserves the contract.
pub struct LocalFileSink;

impl LocalFileSink {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalFileSink {
    fn default() -> Self {
        Self::new()
    }
}

impl FlowSink for LocalFileSink {
    fn write(&self, record: &FlowRecord) -> Result<()> {
        let dir = flows_dir();
        let day = day_utc_now();
        let path = dir.join(format!("{day}.jsonl"));
        record_at(record, &path)
    }
}

// ─── RedisSink (#162 Phase 3) ────────────────────────────────────────
//
// Live-coordination sink: XADD to a Redis Stream. Coexists with
// LocalFileSink via TeeSink — Redis is the coordination substrate,
// files are the audit substrate (see #163 for the compliance-strength
// AuditFileSink and #162's refinement comment on the split).
//
// Opt-in via `DARKMUX_REDIS_URL` env var. When set, the default sink
// becomes `TeeSink([LocalFileSink, RedisSink])`. When unset, the
// default sink stays `LocalFileSink` alone — no Redis dep code runs.
// Stream name defaults to `darkmux:flow`; override via
// `DARKMUX_REDIS_STREAM`.

/// Redis Streams-backed flow sink. Each `write` XADDs the record's
/// JSON-serialized fields to a single stream. Multiple consumers can
/// `XREAD BLOCK` for live updates; consumer groups handle multi-reader
/// fan-out; `MAXLEN ~ N` caps the stream size at the operator's chosen
/// retention.
///
/// **By design ephemeral** — Redis Streams with MAXLEN drop old records.
/// NOT the audit substrate. Pair with a durable sink (LocalFileSink or
/// AuditFileSink) via TeeSink for any operator who needs both
/// coordination AND audit. See #163 + the #162 refinement comment.
pub struct RedisSink {
    client: redis::Client,
    stream: String,
    /// Optional MAXLEN ~ N retention cap. None = unbounded (don't use
    /// in production; the stream grows without bound).
    max_len: Option<usize>,
}

impl RedisSink {
    /// Build a sink connecting to `url` and writing to `stream`. Connection
    /// is not established until the first `write` call (the redis client
    /// is lazy by design).
    pub fn new(url: &str, stream: &str, max_len: Option<usize>) -> Result<Self> {
        let client = redis::Client::open(url)
            .with_context(|| format!("opening Redis connection to {url}"))?;
        Ok(Self {
            client,
            stream: stream.to_string(),
            max_len,
        })
    }
}

impl FlowSink for RedisSink {
    fn write(&self, record: &FlowRecord) -> Result<()> {
        let mut conn = self
            .client
            .get_connection()
            .context("getting Redis connection")?;
        let payload = serde_json::to_string(record)
            .context("serializing FlowRecord for Redis")?;
        // Two-field encoding: `schema` carries the version (so downstream
        // consumers across darkmux versions can handle skew explicitly),
        // `record` carries the JSON-serialized FlowRecord. Single XADD
        // call per write; small payload (~1 KB typical) so MAXLEN trim
        // can run synchronously without affecting latency.
        let fields: &[(&str, &str)] = &[
            ("schema", FLOW_SCHEMA_VERSION),
            ("record", &payload),
        ];
        // XADD <stream> [MAXLEN ~ N] * field value [field value ...]
        let mut cmd = redis::cmd("XADD");
        cmd.arg(&self.stream);
        if let Some(n) = self.max_len {
            cmd.arg("MAXLEN").arg("~").arg(n);
        }
        cmd.arg("*"); // auto-generated ID
        for (k, v) in fields {
            cmd.arg(*k).arg(*v);
        }
        let _: String = cmd
            .query(&mut conn)
            .with_context(|| format!("XADD to Redis stream `{}`", self.stream))?;
        Ok(())
    }
}

// ─── TeeSink (#162 Phase 3) ───────────────────────────────────────────
//
// Compositional sink: writes each record to N child sinks. Errors from
// any single child are logged but don't fail the overall write — the
// audit substrate has to remain durable even when coordination layer
// is degraded. Per the operator-sovereignty contract: surface failures
// loudly via stderr; don't silently lose the audit record.

pub struct TeeSink {
    sinks: Vec<Arc<dyn FlowSink>>,
}

impl TeeSink {
    pub fn new(sinks: Vec<Arc<dyn FlowSink>>) -> Self {
        Self { sinks }
    }
}

impl FlowSink for TeeSink {
    fn write(&self, record: &FlowRecord) -> Result<()> {
        // Best-effort: record per-sink failures but always attempt every
        // sink. Return the first error (so callers can react if they
        // want); log the rest to stderr so the operator sees them.
        let mut first_err: Option<anyhow::Error> = None;
        for (i, sink) in self.sinks.iter().enumerate() {
            if let Err(e) = sink.write(record) {
                eprintln!(
                    "flow::TeeSink: sink #{i} write failed: {e:#}"
                );
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

// ─── Default-sink selection (#162 Phase 3) ────────────────────────────

/// Build the process-wide default sink from env-var configuration.
///
/// - `DARKMUX_REDIS_URL` set (and non-empty) → `TeeSink([LocalFileSink, RedisSink])`
/// - Else → `LocalFileSink` alone (current behavior preserved)
///
/// `DARKMUX_REDIS_STREAM` overrides the stream name (default `darkmux:flow`).
/// `DARKMUX_REDIS_MAXLEN` overrides the retention cap (default 10000;
/// set to `0` for unbounded — not recommended).
///
/// Connection errors at construction degrade gracefully: if Redis is
/// unreachable when the sink builds, the warning logs to stderr and the
/// default sink falls back to LocalFileSink alone. Operators see the
/// connection failure loudly; the audit substrate stays intact.
fn build_default_sink() -> Arc<dyn FlowSink> {
    let local = Arc::new(LocalFileSink::new());

    let redis_url = match std::env::var("DARKMUX_REDIS_URL") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => return local,
    };

    let stream = std::env::var("DARKMUX_REDIS_STREAM")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "darkmux:flow".to_string());

    let max_len = match std::env::var("DARKMUX_REDIS_MAXLEN") {
        Ok(s) => match s.parse::<usize>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => Some(10000),
        },
        Err(_) => Some(10000),
    };

    match RedisSink::new(&redis_url, &stream, max_len) {
        Ok(redis_sink) => {
            eprintln!(
                "flow: Redis sink enabled — url={redis_url} stream={stream} \
                 max_len={max_len:?} (tee'd with local file sink)"
            );
            Arc::new(TeeSink::new(vec![local, Arc::new(redis_sink)]))
        }
        Err(e) => {
            eprintln!(
                "flow: Redis sink construction failed ({e:#}); falling back to \
                 local file sink only. Audit substrate intact."
            );
            local
        }
    }
}

/// Process-wide default sink. Initialized lazily on first call to
/// `record()`; default selection reads env config at init time.
fn default_sink() -> Arc<dyn FlowSink> {
    static SINK: OnceLock<Arc<dyn FlowSink>> = OnceLock::new();
    SINK.get_or_init(build_default_sink).clone()
}

/// Write a record through an explicit sink. Used by tests + future
/// config-driven dispatch paths where the caller picks the sink. The
/// production code path uses `record()` which dispatches through the
/// process-wide default sink.
pub fn record_via(sink: &dyn FlowSink, record: &FlowRecord) -> Result<()> {
    sink.write(record)
}

/// Append `record` to today's per-day JSONL file. Creates the file with a
/// schema header as line 1 if it doesn't exist (written atomically with the
/// first record so a partial file never ends up header-only).
///
/// Concurrent writes: append-on-Unix is atomic up to PIPE_BUF (~4 KB on
/// macOS). Single-line JSONL records are well under this limit, so no
/// explicit locking is needed.
///
/// **Phase 1 refactor (#162):** this function now dispatches through
/// `FlowSink`. The default sink is `LocalFileSink`, which preserves
/// the original behavior. No callers should see a behavior change.
pub fn record(record: FlowRecord) -> Result<()> {
    default_sink().write(&record)
}

/// Internal entry point writing to an explicit path. Used by tests and the
/// public `record()` wrapper delegates here after resolving the path.
///
/// Atomic-first-write semantics: when the file doesn't exist yet, the
/// schema header AND the first record are written in a single `write_all`
/// call against an exclusively-created handle (`create_new(true)`). This
/// closes two race classes the prior naive open+metadata-check pattern had:
///
///   1. **TOCTOU on header-needed check** — two concurrent processes both
///      seeing `len()==0` and both writing headers. Fixed: `create_new` is
///      atomic at the syscall level; only one process wins the create.
///   2. **Crash between header and record** — header-only files when the
///      process dies after writing line 1 but before line 2. Fixed: both
///      lines join into one buffer, one `write_all` syscall.
///
/// Concurrent appenders after the file exists: append-on-Unix is atomic
/// up to PIPE_BUF (~4 KB on macOS); a single-line JSONL record is well
/// under that, so no explicit locking is needed for the append case.
///
/// `sync_all()` is called after both write paths so audit-log durability
/// survives power loss / crash between record emission and consumer read.
pub(crate) fn record_at(record: &FlowRecord, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating flows dir {}", parent.display()))?;
        }
    }

    let darkmux_version = env!("CARGO_PKG_VERSION");
    let schema_header = serde_json::json!({
        "_type": "schema",
        "version": FLOW_SCHEMA_VERSION,
        "darkmux_version": darkmux_version,
    });
    let header_line = serde_json::to_string(&schema_header)?;
    let record_line = serde_json::to_string(record)?;

    // Try the atomic-create path: we win the create race → write header +
    // record together. If file already exists (other process or earlier
    // call), fall through to append-only.
    match fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(format!("{header_line}\n{record_line}\n").as_bytes())
                .with_context(|| format!("writing initial flow log {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("syncing flow log {}", path.display()))?;
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(path)
                .with_context(|| format!("opening flow log for append {}", path.display()))?;
            writeln!(file, "{record_line}")
                .with_context(|| format!("appending to flow log {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("syncing flow log {}", path.display()))?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("creating flow log {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::env;
    use tempfile::TempDir;

    #[serial_test::serial]
    #[test]
    fn creates_file_with_schema_header_on_first_record() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-15.jsonl");

        let record = FlowRecord {
            ts: "2025-01-15T12:34:56Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Dispatch,
            action: "ran".to_string(),
            handle: "test-1".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
        };

        record_at(&record, &path).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 lines: header + record");

        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["_type"], "schema");

        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["level"], "info");
    }

    #[serial_test::serial]
    #[test]
    fn appends_to_existing_file_without_re_emitting_header() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-15.jsonl");

        let r = |action: &str| FlowRecord {
            ts: "2025-01-15T12:34:56Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Dispatch,
            action: action.to_string(),
            handle: "test".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
        };

        record_at(&r("first"), &path).unwrap();
        record_at(&r("second"), &path).unwrap();
        record_at(&r("third"), &path).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 4, "expected 4 lines: header + 3 records");

        // Header should be exactly once.
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["_type"], "schema");

        // Records 2 and 3 are plain flow records, no `_type: schema`.
        for line in &lines[1..] {
            let line: Value = serde_json::from_str(line).unwrap();
            assert!(
                line.get("_type").is_none(),
                "record line should not contain _type"
            );
        }
    }

    #[test]
    fn record_serializes_with_expected_shape() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-15.jsonl");

        let record = FlowRecord {
            ts: "2025-06-01T08:00:00Z".to_string(),
            level: Level::Warn,
            category: Category::Audit,
            tier: Tier::Local,
            stage: Stage::Estimate,
            action: "budget_check".to_string(),
            handle: "handle-42".to_string(),
            sprint_id: Some("sp-100".to_string()),
            session_id: Some("sess-abc".to_string()),
            source: Some("estimator".to_string()),
            model: None,
            reasoning: None,
            mission_id: None,
        };

        record_at(&record, &path).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        // Second line is the record (first is schema header).
        let lines: Vec<&str> = contents.lines().collect();
        let rec_line = lines[1];

        // Parse as JSON and check fields.
        let parsed: Value = serde_json::from_str(rec_line).unwrap();

        assert_eq!(parsed["ts"], "2025-06-01T08:00:00Z");
        assert_eq!(parsed["level"], "warn");
        assert_eq!(parsed["category"], "audit");
        assert_eq!(parsed["tier"], "local");
        assert_eq!(parsed["stage"], "estimate");
        assert_eq!(parsed["action"], "budget_check");
        assert_eq!(parsed["handle"], "handle-42");

        // Optional fields should be present (not omitted) when set.
        let sprint_id = parsed.get("sprint_id").expect("expected sprint_id");
        assert_eq!(sprint_id, "sp-100");

        let session_id = parsed.get("session_id").expect("expected session_id");
        assert_eq!(session_id, "sess-abc");

        let source = parsed.get("source").expect("expected source");
        assert_eq!(source, "estimator");

        // Round-trip: parse back into FlowRecord.
        let roundtrip: FlowRecord = serde_json::from_str(rec_line).unwrap();
        assert_eq!(roundtrip.action, "budget_check");
        assert_eq!(roundtrip.handle, "handle-42");
    }

    #[serial_test::serial]
    #[test]
    fn record_at_uses_explicit_path() {
        let tmp = TempDir::new().unwrap();

        // Ensure DARKMUX_FLOWS_DIR is NOT set (or cleared) so we don't
        // accidentally write to an unexpected location.
        let prev = env::var("DARKMUX_FLOWS_DIR").ok();

        record_at(
            &FlowRecord {
                ts: "2025-03-21T14:00:00Z".to_string(),
                level: Level::Trace,
                category: Category::Review,
                tier: Tier::Frontier,
                stage: Stage::Scope,
                action: "scope_review".to_string(),
                handle: "ex-path-1".to_string(),
                sprint_id: None,
                session_id: None,
                source: Some("reviewer".to_string()),
                model: None,
                reasoning: None,
                mission_id: None,
            },
            &tmp.path().join("custom.jsonl"),
        )
        .unwrap();

        // Restore env var.
        match prev {
            Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
            None => env::remove_var("DARKMUX_FLOWS_DIR"),
        }

        let contents = fs::read_to_string(tmp.path().join("custom.jsonl")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2); // header + record

        let parsed: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed["action"], "scope_review");
    }

    #[serial_test::serial]
    #[test]
    fn optional_fields_omit_when_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-01.jsonl");

        let record = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Ship,
            action: "deploy".to_string(),
            handle: "ship-1".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
        };

        record_at(&record, &path).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        let rec_line = lines[1];

        // Optional fields should NOT appear when None.
        let parsed: Value = serde_json::from_str(rec_line).unwrap();

        // Verify keys don't exist (not null, absent entirely).
        assert!(parsed.get("sprint_id").is_none());
        assert!(parsed.get("session_id").is_none());
        assert!(parsed.get("source").is_none());

        // Required fields must be present.
        assert!(parsed.get("ts").is_some());
        assert!(parsed.get("level").is_some());
        assert!(parsed.get("action").is_some());
    }

    #[serial_test::serial]
    #[test]
    fn flows_dir_respects_env_override() {
        let tmp = TempDir::new().unwrap();

        // SAFETY: serialized via `#[serial_test::serial]` on every test that
        // mutates this env var. Outside that lock, `set_var` is unsafe in
        // 2024 edition (race with other readers); serial-tests serializes it.
        let prev = env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { env::set_var("DARKMUX_FLOWS_DIR", tmp.path()); }

        let rec = FlowRecord {
            ts: "2025-04-10T10:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Review,
            action: "env_test".to_string(),
            handle: "ev-1".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
        };

        // Capture the day-key BEFORE calling record() so a midnight-UTC
        // crossing between record() and the assertion doesn't make the
        // file appear at a different name than we check. (record() reads
        // day_utc_now() once for the file path; we read it once too; both
        // reads land in the same wall-clock window typically <1ms apart.)
        let day_before = day_utc_now();
        super::record(rec).unwrap();
        let day_after = day_utc_now();

        // SAFETY: same — serialized via the test attribute.
        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                None => env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        // Accept either day's file (handles the rare midnight crossing).
        let candidates = if day_before == day_after {
            vec![tmp.path().join(format!("{day_before}.jsonl"))]
        } else {
            vec![
                tmp.path().join(format!("{day_before}.jsonl")),
                tmp.path().join(format!("{day_after}.jsonl")),
            ]
        };
        let found = candidates.iter().find(|p| p.exists()).cloned();
        assert!(
            found.is_some(),
            "file should exist in env-override dir under {} or {}",
            day_before,
            day_after
        );

        let contents = fs::read_to_string(found.unwrap()).unwrap();
        assert!(contents.contains("env_test"));
    }

    #[test]
    fn epoch_to_yyyymmdd_known_dates() {
        // Unix epoch start
        let (y, m, d) = epoch_to_yyyymmdd(0);
        assert_eq!((y, m, d), (1970, 1, 1));

        // Leap year: 2024-02-29
        let (y, m, d) = epoch_to_yyyymmdd(1_709_164_800);
        assert_eq!((y, m, d), (2024, 2, 29));

        // Year boundary: 2025-01-01 = epoch 1735689600
        let (y, m, d) = epoch_to_yyyymmdd(1_735_689_600);
        assert_eq!((y, m, d), (2025, 1, 1));

        // Mid-year: 2024-07-04 = epoch 1_720_051_200
        let (y, m, d) = epoch_to_yyyymmdd(1_720_051_200);
        assert_eq!((y, m, d), (2024, 7, 4));
    }

    #[test]
    fn epoch_to_hhmmss_known_times() {
        // Midnight
        assert_eq!(epoch_to_hhmmss(0), (0, 0, 0));
        // 2024-01-01 00:00:00 UTC
        assert_eq!(epoch_to_hhmmss(1_704_067_200), (0, 0, 0));
        // 2024-01-01 12:34:56 UTC = epoch start + 12*3600 + 34*60 + 56 = 1_704_067_200 + 45_296
        assert_eq!(epoch_to_hhmmss(1_704_067_200 + 45_296), (12, 34, 56));
        // 23:59:59 boundary: midnight - 1 second
        assert_eq!(epoch_to_hhmmss(86_400 - 1), (23, 59, 59));
        // Mid-day check: epoch 1_720_094_400 = 2024-07-04 12:00:00 UTC
        // (epoch 1_720_051_200 is 2024-07-04 00:00:00 UTC; +43_200s = noon)
        assert_eq!(epoch_to_hhmmss(1_720_051_200 + 43_200), (12, 0, 0));
    }

    #[test]
    fn ts_utc_now_returns_iso8601_datetime() {
        // Schema 1.1: ts must be full datetime with time-of-day, not just a date.
        let ts = ts_utc_now();
        let bytes = ts.as_bytes();
        assert_eq!(ts.len(), 20, "expected YYYY-MM-DDTHH:MM:SSZ (20 chars), got {ts:?}");
        assert_eq!(bytes[4], b'-');
        assert_eq!(bytes[7], b'-');
        assert_eq!(bytes[10], b'T');
        assert_eq!(bytes[13], b':');
        assert_eq!(bytes[16], b':');
        assert_eq!(bytes[19], b'Z');
        // Digits in the expected positions
        for &i in &[0usize, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18] {
            assert!(
                bytes[i].is_ascii_digit(),
                "expected digit at index {i} in {ts:?}",
            );
        }
    }

    #[test]
    fn day_utc_now_returns_date_only() {
        // day_utc_now() is for file naming — must stay YYYY-MM-DD regardless
        // of the schema bump on ts_utc_now().
        let day = day_utc_now();
        let bytes = day.as_bytes();
        assert_eq!(day.len(), 10, "expected YYYY-MM-DD (10 chars), got {day:?}");
        assert_eq!(bytes[4], b'-');
        assert_eq!(bytes[7], b'-');
        for &i in &[0usize, 1, 2, 3, 5, 6, 8, 9] {
            assert!(
                bytes[i].is_ascii_digit(),
                "expected digit at index {i} in {day:?}",
            );
        }
    }

    // ─── #162 Phase 1: FlowSink trait ────────────────────────────────

    #[test]
    fn local_file_sink_writes_through_to_per_day_jsonl() {
        // LocalFileSink should produce the same on-disk result as the
        // historical `record_at` path — preserving behavior under the
        // trait abstraction is the whole point of Phase 1.
        use std::env;
        let tmp = TempDir::new().unwrap();
        let prev = env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { env::set_var("DARKMUX_FLOWS_DIR", tmp.path()); }

        let sink = LocalFileSink::new();
        let rec = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "test".to_string(),
            handle: "h".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
        };
        sink.write(&rec).unwrap();

        // Result must be a per-day JSONL file at flows_dir() with the
        // record's content as line 2 (line 1 is the schema header).
        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        assert!(path.exists(), "sink should have created per-day file");
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert!(lines[0].contains("\"_type\":\"schema\""), "line 1 = header");
        assert!(lines[1].contains("\"action\":\"test\""), "line 2 = record");

        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                None => env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    /// Test-only sink that captures records in memory. Used to verify the
    /// trait contract without filesystem interaction.
    struct InMemorySink {
        captured: std::sync::Mutex<Vec<FlowRecord>>,
    }
    impl InMemorySink {
        fn new() -> Self {
            Self { captured: std::sync::Mutex::new(Vec::new()) }
        }
        fn count(&self) -> usize {
            self.captured.lock().unwrap().len()
        }
    }
    impl FlowSink for InMemorySink {
        fn write(&self, record: &FlowRecord) -> Result<()> {
            self.captured.lock().unwrap().push(record.clone());
            Ok(())
        }
    }

    #[test]
    fn record_via_dispatches_through_explicit_sink() {
        // The trait's contract: any FlowSink impl receives the record on
        // write. record_via is the public extension point for callers
        // that want to override the default LocalFileSink (tests today;
        // RedisSink + TeeSink in Phase 3 of #162).
        let sink = InMemorySink::new();
        let rec = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "explicit-sink".to_string(),
            handle: "h".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
        };

        record_via(&sink, &rec).unwrap();
        record_via(&sink, &rec).unwrap();
        assert_eq!(sink.count(), 2);
    }

    #[test]
    fn tee_sink_writes_to_all_children() {
        // #162 Phase 3: TeeSink composes N sinks. Each child receives
        // the record. This is the canonical compliant deployment shape
        // ([LocalFileSink, RedisSink] in production); the test uses
        // two InMemorySink test doubles to verify the trait contract.
        let a = Arc::new(InMemorySink::new());
        let b = Arc::new(InMemorySink::new());
        let tee = TeeSink::new(vec![
            a.clone() as Arc<dyn FlowSink>,
            b.clone() as Arc<dyn FlowSink>,
        ]);

        let rec = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "tee-test".to_string(),
            handle: "h".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
        };
        tee.write(&rec).unwrap();
        tee.write(&rec).unwrap();

        assert_eq!(a.count(), 2);
        assert_eq!(b.count(), 2);
    }

    /// Test-only sink that always returns an error on write. Used to
    /// verify TeeSink's best-effort semantics — one failing child
    /// shouldn't prevent the others from receiving the record.
    struct FailingSink;
    impl FlowSink for FailingSink {
        fn write(&self, _record: &FlowRecord) -> Result<()> {
            anyhow::bail!("simulated sink failure for test")
        }
    }

    #[test]
    fn tee_sink_continues_writing_when_one_child_fails() {
        // The audit substrate must remain durable even when the
        // coordination layer (Redis) is unreachable. TeeSink logs the
        // failure and continues writing to other sinks. First error
        // bubbles up to the caller; subsequent sinks still receive.
        let good = Arc::new(InMemorySink::new());
        let bad = Arc::new(FailingSink);
        let tee = TeeSink::new(vec![
            bad as Arc<dyn FlowSink>,
            good.clone() as Arc<dyn FlowSink>,
        ]);

        let rec = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "tee-fail".to_string(),
            handle: "h".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
        };
        let err = tee.write(&rec).unwrap_err();
        // Caller sees the error (so they can react if they want)
        assert!(err.to_string().contains("simulated sink failure"));
        // But the audit substrate still received the record
        assert_eq!(good.count(), 1);
    }

    #[test]
    fn record_default_path_uses_local_file_sink() {
        // The public `record()` should dispatch through the default sink
        // and produce on-disk output (behavior-equivalent to pre-#162).
        // We can't easily intercept the default sink from a test, but we
        // can verify the round trip: write via record(), read from
        // flows_dir(), see the record.
        use std::env;
        let tmp = TempDir::new().unwrap();
        let prev = env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { env::set_var("DARKMUX_FLOWS_DIR", tmp.path()); }

        let rec = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "default-path".to_string(),
            handle: "h".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
        };
        super::record(rec).unwrap();

        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        assert!(path.exists(), "default sink should have written to {}", path.display());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"action\":\"default-path\""));

        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                None => env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    #[test]
    fn flow_schema_version_is_1_3_0() {
        // Pin the schema version so an accidental rename can't ship silently;
        // any bump beyond this should be a deliberate code change paired with
        // an update to this assertion (and corresponding viewer EXPECTED_*
        // bump if the change is breaking).
        //
        // Version history:
        //   1.2.0 — added optional `model` field (#106, Sprint 4 of #104)
        //   1.3.0 — added optional `reasoning` and `mission_id` fields and a
        //           new `Stage::TierDecision` variant (#136). Minor bump:
        //           older viewers can safely ignore the new fields and will
        //           see the new stage value as an unknown literal — no data
        //           loss, no breaking parse.
        assert_eq!(FLOW_SCHEMA_VERSION, "1.3.0");
    }

    #[test]
    fn stage_tier_decision_round_trips_as_kebab_case() {
        // Schema 1.3 introduced Stage::TierDecision and changed the
        // serde rename from `lowercase` to `kebab-case`. Both directions
        // (serialize + deserialize) must agree for the new variant AND
        // for the existing single-word variants (which should be no-ops).
        for (variant, expected) in [
            (Stage::Scope, "scope"),
            (Stage::Estimate, "estimate"),
            (Stage::Dispatch, "dispatch"),
            (Stage::Review, "review"),
            (Stage::Ship, "ship"),
            (Stage::Retrospect, "retrospect"),
            (Stage::TierDecision, "tier-decision"),
        ] {
            let serialized = serde_json::to_string(&variant).unwrap();
            assert_eq!(serialized.trim_matches('"'), expected,
                "{variant:?} should serialize as {expected}");
            let parsed: Stage = serde_json::from_str(&serialized).unwrap();
            // Round-trip equality via Debug (Stage doesn't derive PartialEq).
            assert_eq!(format!("{parsed:?}"), format!("{variant:?}"));
        }
    }

    #[test]
    fn reasoning_and_mission_id_omit_when_none() {
        // schema_serialize_omit_when_none-style guarantee for the new
        // schema-1.3 fields. When both are None, the serialized JSON
        // must NOT contain "reasoning":null or "mission_id":null.
        let rec = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "test".to_string(),
            handle: "h".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
        };
        let serialized = serde_json::to_string(&rec).unwrap();
        assert!(!serialized.contains("reasoning"),
            "absent reasoning leaked into JSON: {serialized}");
        assert!(!serialized.contains("mission_id"),
            "absent mission_id leaked into JSON: {serialized}");
    }

    #[test]
    fn schema_header_contains_version_and_darkmux_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-01.jsonl");

        record_at(
            &FlowRecord {
                ts: "2025-01-01T00:00:00Z".to_string(),
                level: Level::Info,
                category: Category::Work,
                tier: Tier::Operator,
                stage: Stage::Dispatch,
                action: "init".to_string(),
                handle: "schema-check".to_string(),
                sprint_id: None,
                session_id: None,
                source: None,
                model: None,
                reasoning: None,
                mission_id: None,
            },
            &path,
        )
        .unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        let header: Value = serde_json::from_str(lines[0]).unwrap();

        assert_eq!(header["version"], FLOW_SCHEMA_VERSION);
        // CARGO_PKG_VERSION is set by cargo; check it's a non-empty string.
        let ver: &str = header["darkmux_version"].as_str().unwrap();
        assert!(!ver.is_empty());
    }
}
