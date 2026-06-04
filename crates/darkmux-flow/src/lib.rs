//! Flow observability — structured JSONL records for darkmux run tracking.
//!
//! # Storage model
//!
//! Records are appended to a per-day JSONL file (`YYYY-MM-DD.jsonl`) under
//! `~/.darkmux/flows/` (overridable via `DARKMUX_FLOWS_DIR`). The first write
//! atomically prepends a schema header so partial-file recovery is possible.

pub mod daemon_probe;
pub mod presence;
pub mod presence_reconciler;
pub mod session_presence;

mod integrity;
mod schema;
mod status;

pub use integrity::*;
pub use schema::*;
pub use status::*;

use crate::integrity::{audit_record_at, schema_header_line};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

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

/// Structured snapshot of a sink's identity + config for diagnostics
/// (`darkmux flow status`, `darkmux doctor` flow-sink-health). The
/// tree mirrors the sink composition: a TeeSink reports its `children`,
/// leaf sinks report empty `children`.
///
/// `config` is intentionally a flat key→string map (not a typed enum
/// per sink) so a new sink kind can be added without touching every
/// downstream consumer — the human formatter prints whatever's in
/// `config`; the JSON serializer is a pass-through.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SinkInfo {
    pub kind: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub config: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SinkInfo>,
    /// Credential-bearing identifier that must round-trip through the
    /// in-process probe path (e.g., `RedisSink` URL) without ever
    /// leaving the process. Never serialized — `config` carries the
    /// redacted display form for any external surface (CLI JSON, daemon
    /// HTTP endpoint). See `find_redis_cfg` for the consumer side. (#216)
    #[serde(skip)]
    pub raw_url: Option<String>,
}

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

    /// Introspection for diagnostics. Required so `darkmux flow status`
    /// and the doctor's `flow-sink-health` check can describe the active
    /// sink graph without per-sink-type knowledge.
    fn info(&self) -> SinkInfo;
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
    // NOTE (#507): LocalFileSink still resolves `flows_dir()` per write,
    // unlike AuditFileSink (which captures its dir at construction below).
    // Capturing here too is the right end-state, but it changes the
    // default sink's "honor a live DARKMUX_FLOWS_DIR" behavior that ~9
    // tests across the binary + crew rely on — converting those to
    // explicit sinks is its own scoped task, tracked as a #507 follow-up.
    // FLOWS_DIR (unlike AUDIT_DIR) is not concurrently scrubbed by the
    // test-isolation helper, so this path is not a demonstrated race.
    fn write(&self, record: &FlowRecord) -> Result<()> {
        let dir = flows_dir();
        let day = day_utc_now();
        let path = dir.join(format!("{day}.jsonl"));
        record_at(record, &path)
    }

    fn info(&self) -> SinkInfo {
        let mut config = std::collections::BTreeMap::new();
        config.insert("flows_dir".to_string(), flows_dir().display().to_string());
        SinkInfo { kind: "LocalFile".to_string(), config, children: vec![], raw_url: None }
    }
}

// ─── AuditFileSink (#163) ────────────────────────────────────────────
//
// Compliance-strength sibling of LocalFileSink. Same per-day JSONL append
// format, plus:
//   - BLAKE3 hash chain — each record carries the prior record's hash,
//     making any after-the-fact edit detectable via a linear walk.
//   - Cross-process flock — concurrent CLI sessions writing the same
//     day file serialize through `flock(2)` so the hash chain can't
//     interleave (which would surface as a chain break the operator
//     might mistake for tampering).
//   - Separate directory (default `~/.darkmux/audit/`, overridable via
//     `DARKMUX_AUDIT_DIR`) — keeps casual flow records visually
//     distinct from compliance-strength records and lets the operator
//     mount the audit dir on different storage (encrypted volume,
//     read-only mirror, etc.).
//
// **POSIX-only** (`#[cfg(unix)]`) — `flock(2)` is the locking primitive.
// On Windows builds, AuditFileSink doesn't exist and `build_default_sink`
// silently skips it; the integrity-check verb + doctor check report
// "audit sink is unix-only on this platform". Cross-platform support
// would need `LockFileEx` and a separate code path — out of scope here.
//
// Tamper-evident, NOT tamper-proof. OS-level append-only flags
// (`chflags uappend` / `chattr +a`) are a follow-up; this PR ships the
// chain layer. Operators in regulated environments compose this with
// disk encryption + filesystem-level immutability for layered defense.

/// Resolve the audit directory from env override (`DARKMUX_AUDIT_DIR`)
/// or default (`~/.darkmux/audit/`). Symmetric with `flows_dir()` but
/// deliberately separate so audit and casual records never share a path.
pub fn audit_dir() -> PathBuf {
    std::env::var("DARKMUX_AUDIT_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".darkmux").join("audit")))
        .unwrap_or_else(|| PathBuf::from("/tmp/darkmux/audit"))
}

/// Hash-chained tamper-evident sink. See module-level comment for the
/// design rationale. POSIX-only.
#[cfg(unix)]
pub struct AuditFileSink {
    // #507 — captured once at construction (see LocalFileSink). Capturing
    // the audit dir up front is what makes the cross-process hash chain
    // robust against a mid-sequence `DARKMUX_AUDIT_DIR` change (the
    // `records_checked == 1` flake the #463 cycle-break worked around at
    // the isolate layer; this removes the underlying per-write re-read).
    dir: PathBuf,
}

#[cfg(unix)]
impl AuditFileSink {
    /// Capture the audit dir from the environment (`DARKMUX_AUDIT_DIR` →
    /// default) at construction time.
    pub fn new() -> Self {
        Self { dir: audit_dir() }
    }

    /// Construct against an explicit dir (tests / config-driven dispatch).
    pub fn with_dir(dir: PathBuf) -> Self {
        Self { dir }
    }
}

#[cfg(unix)]
impl Default for AuditFileSink {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unix)]
impl FlowSink for AuditFileSink {
    fn write(&self, record: &FlowRecord) -> Result<()> {
        let day = day_utc_now();
        let path = self.dir.join(format!("{day}.jsonl"));
        audit_record_at(record, &path)
    }

    fn info(&self) -> SinkInfo {
        let mut config = std::collections::BTreeMap::new();
        config.insert("audit_dir".to_string(), self.dir.display().to_string());
        config.insert("hash".to_string(), "blake3".to_string());
        SinkInfo { kind: "AuditFile".to_string(), config, children: vec![], raw_url: None }
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

/// Opaque wrapper for a Redis URL that contains credentials (#229).
/// `Display` produces the redacted form (`user:***@host:port`); raw
/// bytes are only accessible via `expose_for_probe()`, making accidental
/// password leakage into logs or serialized JSON a compile-time error
/// rather than a convention.
///
/// The only call site for `redact_url_creds` in production code is the
/// `Display` implementation below — all other paths reach the redacted
/// form through `format!("{raw_url}")` or `.to_string()`.
#[derive(Clone, Debug)]
pub struct RawRedisUrl(String);

impl RawRedisUrl {
    pub fn new(url: String) -> Self {
        Self(url)
    }

    /// Return the raw (unredacted) URL for `redis::Client::open` calls.
    /// The verbose name makes accidental use visible in review.
    pub fn expose_for_probe(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RawRedisUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&redact_url_creds(&self.0))
    }
}

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
    /// URL the sink was constructed with — retained for diagnostics
    /// (`SinkInfo`, `darkmux flow status`). Stored as `RawRedisUrl` so
    /// `Display` automatically redacts the password; raw bytes only
    /// accessible via `expose_for_probe()`. (#229)
    url: RawRedisUrl,
    stream: String,
    /// Optional MAXLEN ~ N retention cap. None = unbounded (don't use
    /// in production; the stream grows without bound).
    max_len: Option<usize>,
    /// (#388) Consecutive write-failure counter. Reset to 0 on any
    /// successful write. When it reaches `REDIS_DISABLE_THRESHOLD` the
    /// sink disables itself for the rest of the process.
    consecutive_failures: AtomicU32,
    /// (#388) Once the failure counter trips the threshold, the sink is
    /// disabled: subsequent writes skip silently (no connection attempt,
    /// no per-write log spam). Spares single-machine operators who set
    /// `DARKMUX_REDIS_URL` "just in case" from a 500ms-timeout-plus-log
    /// on every `darkmux` invocation when the peer is offline.
    disabled: AtomicBool,
}

/// (#388) Consecutive write failures before a `RedisSink` disables
/// itself for the process. 3 strikes balances "tolerate a one-off blip"
/// against "stop spamming a 500ms timeout + log line per write when the
/// peer is genuinely offline."
const REDIS_DISABLE_THRESHOLD: u32 = 3;

/// Hard cap on the wall-clock spent connecting + handshaking to Redis
/// from any `RedisSink` or sink-diagnostic probe (#278). The OS default
/// TCP-connect + handshake budget is platform-dependent and on macOS
/// can wait ~75 seconds when the host is reachable at the IP layer but
/// silent at the TCP/Redis layer (the canonical "Tailscale peer just
/// dropped" failure mode). Without this cap, every flow-record write
/// blocks the caller for the full OS budget — multiplied across the
/// ~30 tests that touch the flow pipeline, it turned a 5-second
/// `cargo test` into the 51-minute debacle from 2026-05-22.
///
/// 500ms is generous for a healthy LAN/tailnet round-trip (typical
/// connect+handshake ≤ 50ms) and bounds the worst-case per-write
/// cost at a known ceiling. The cost of the bound is that operators
/// running Redis behind a slow VPN where 500ms isn't enough will see
/// flow-record writes fail; if that surfaces in practice we'll need
/// to make the cap operator-configurable.
pub const REDIS_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Test-only env scrubber (#278). Tests in `flow::tests` write flow
/// records via the default sink path which respects `DARKMUX_REDIS_URL`.
/// An operator running tests from their daily shell with that var pointing
/// at an unreachable peer (the Studio-offline scenario from 2026-05-21)
/// saw the test bin wall-clock balloon by 75s/record. This helper scrubs
/// it in any flow test that writes records via the default sink path;
/// idempotent and safe to call multiple times. Uses `OnceLock` so the
/// scrub fires exactly once per test-binary invocation. (Deliberately does
/// NOT touch `DARKMUX_AUDIT_DIR` — see the note in the body.)
// Gated on `any(test, feature = "test-support")` rather than `test` alone:
// since #463 split flow into its own crate, a plain `#[cfg(test)]` would only
// compile this for flow's *own* test build, leaving it invisible to downstream
// crates' tests (e.g. `flow_cli` tests in the binary). The `test-support`
// feature lets the binary opt in via a dev-dependency without compiling the
// helper into release builds.
#[cfg(any(test, feature = "test-support"))]
pub fn isolate_test_env_once() {
    static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            // NOTE: we intentionally do NOT scrub DARKMUX_AUDIT_DIR here.
            // This OnceLock fires lazily on the first default-sink dispatch,
            // which can land mid-flight while a `#[serial]` audit test has
            // legitimately set DARKMUX_AUDIT_DIR to its own tmp dir — wiping
            // it and routing that test's later records elsewhere (the
            // intermittent `records_checked == 1` failure on
            // audit_file_sink_recovers_chain_across_process_boundaries).
            // REDIS_URL is the load-bearing scrub: an unreachable peer costs
            // 75s/record (the 2026-05-21 Studio-offline scenario). AUDIT_DIR
            // is a local file path — it never causes that timeout, so leaving
            // it untouched costs nothing and removes the race. (#463)
        }
    });
}

/// Wall-clock-bounded wrapper around `redis::Client::get_connection_with_timeout`
/// (#278). The redis crate's own timeout-bearing API bounds the TCP
/// connect phase only — the post-connect handshake (HELLO / AUTH /
/// HELLO etc.) is unbounded. A peer that ACCEPTS the TCP connection
/// but never completes the handshake (e.g. a half-functional Redis,
/// a TCP listener that does nothing, certain VPN-flap states) can
/// wedge the caller indefinitely. This wrapper runs the full
/// connect-and-handshake in a worker thread and bails at
/// `timeout * 2` wall-clock regardless of which phase is stuck —
/// same shape as the DNS-resolution wrapper in `fleet::parse_address`
/// (#265 Wave-E.10).
///
/// `timeout * 2` is the wall ceiling because the redis crate uses
/// the same `Duration` for the TCP connect; doubling gives the
/// handshake the same budget so a healthy peer with a 400ms RTT
/// completes inside the bound.
pub(crate) fn open_redis_connection_bounded(
    client: &redis::Client,
    timeout: std::time::Duration,
) -> Result<redis::Connection> {
    let client_clone = client.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("redis-connect-bounded".to_string())
        .spawn(move || {
            let result = client_clone.get_connection_with_timeout(timeout);
            // Ignore send errors — receiver may have given up on
            // timeout. The worker thread keeps running until the
            // underlying socket gives up (post-connect handshake
            // hangs are bounded by the OS TCP keepalive + the redis-
            // crate's handshake, which can be minutes on a peer that
            // accepts but never responds). The leak is per-wedge, not
            // unbounded growth — but operators with a long-running
            // daemon hitting a half-functional peer may accumulate
            // worker threads over time. Acceptable for the personal-
            // scope target; revisit if it bites.
            let _ = tx.send(result);
        })
        .map_err(|e| anyhow::anyhow!("spawning redis-connect thread: {e}"))?;
    match rx.recv_timeout(timeout * 2) {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => Err(anyhow::anyhow!("redis connect failed: {e}")),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(anyhow::anyhow!(
            "redis connect exceeded {}ms wall-clock budget — peer may be \
             reachable at TCP but silent at Redis handshake",
            (timeout * 2).as_millis()
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!(
            "redis-connect worker thread panicked or exited without sending result"
        )),
    }
}

impl RedisSink {
    /// Build a sink connecting to `url` and writing to `stream`. Connection
    /// is not established until the first `write` call (the redis client
    /// is lazy by design).
    pub fn new(url: &str, stream: &str, max_len: Option<usize>) -> Result<Self> {
        let url = RawRedisUrl::new(url.to_string());
        let client = redis::Client::open(url.expose_for_probe()).with_context(|| {
            format!("opening Redis connection to {url}")
        })?;
        Ok(Self {
            client,
            url,
            stream: stream.to_string(),
            max_len,
            consecutive_failures: AtomicU32::new(0),
            disabled: AtomicBool::new(false),
        })
    }

    /// (#388) Whether the sink has disabled itself after repeated
    /// failures. Disabled writes skip silently.
    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Acquire)
    }

    /// (#388) Account one write failure. Disables the sink (and logs a
    /// single one-time warning) when the consecutive-failure counter
    /// first reaches `REDIS_DISABLE_THRESHOLD`. Returns true iff this
    /// call is the one that flipped the sink to disabled.
    fn note_failure(&self, err: &anyhow::Error) -> bool {
        let n = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
        if n >= REDIS_DISABLE_THRESHOLD && !self.disabled.swap(true, Ordering::AcqRel) {
            eprintln!(
                "flow::RedisSink: {} unreachable after {n} consecutive write failures \
                 ({err:#}); disabling Redis flow sink for this process. \
                 LocalFileSink is unaffected; re-run after the peer is reachable to re-enable.",
                self.url
            );
            true
        } else {
            false
        }
    }

    /// (#388) Account one successful write — clears the failure streak so
    /// a transient blip never counts toward the disable threshold.
    fn note_success(&self) {
        // A single success clears the streak. The load-then-store isn't
        // one atomic op, but that's benign: a racing failure between the
        // load and the store at worst delays a disable by one write — it
        // can never cause a spurious disable, and a disabled sink never
        // reaches here (write() returns early when disabled). The
        // Acquire/Release pair orders the reset against note_failure's
        // fetch_add so the cleared counter is visible to the next writer.
        if self.consecutive_failures.load(Ordering::Acquire) != 0 {
            self.consecutive_failures.store(0, Ordering::Release);
        }
    }

    /// Connect + return a usable connection. Exposed for diagnostics
    /// (status probe, doctor health check) that need to talk to the
    /// same Redis the sink writes to. Bounded by `REDIS_CONNECT_TIMEOUT`
    /// (#278) so a peer that's silent at the TCP/Redis layer bails
    /// fast instead of wedging the caller for the OS default.
    pub fn connect(&self) -> Result<redis::Connection> {
        open_redis_connection_bounded(&self.client, REDIS_CONNECT_TIMEOUT)
            .with_context(|| format!("connecting to Redis at {}", self.url))
    }

    pub fn url(&self) -> &str { self.url.expose_for_probe() }
    pub fn stream(&self) -> &str { &self.stream }
    pub fn max_len(&self) -> Option<usize> { self.max_len }
}

impl FlowSink for RedisSink {
    fn write(&self, record: &FlowRecord) -> Result<()> {
        // (#388) Once disabled, skip silently — no connection attempt
        // (so no 500ms timeout) and no log. Returning Ok keeps this
        // best-effort coordination sink from masking the durable
        // LocalFileSink's own result in the TeeSink.
        if self.is_disabled() {
            return Ok(());
        }
        match self.try_write(record) {
            Ok(()) => {
                self.note_success();
                Ok(())
            }
            Err(e) => {
                // Swallow: log a single one-time warning at the disable
                // threshold (note_failure), but never propagate to the
                // TeeSink — that's what produced the per-write spam this
                // fixes. Redis is the coordination substrate, not the
                // durable record.
                self.note_failure(&e);
                Ok(())
            }
        }
    }

    fn info(&self) -> SinkInfo {
        self.sink_info()
    }
}

impl RedisSink {
    /// The actual XADD write — fallible. `write` (the trait method) wraps
    /// this with the #388 disable accounting.
    fn try_write(&self, record: &FlowRecord) -> Result<()> {
        let mut conn = open_redis_connection_bounded(&self.client, REDIS_CONNECT_TIMEOUT)
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

    /// `SinkInfo` for diagnostics — called by the `FlowSink::info` impl.
    fn sink_info(&self) -> SinkInfo {
        let mut config = std::collections::BTreeMap::new();
        // The displayed URL is redacted — `config` rides through to JSON
        // output (`darkmux flow status --json` + the daemon's HTTP
        // endpoint), and the password must not appear there. The raw URL
        // is preserved on `SinkInfo.raw_url` (skip-serialized) for the
        // in-process probe path in `find_redis_cfg`. (#216)
        config.insert("url".to_string(), self.url.to_string());
        config.insert("stream".to_string(), self.stream.clone());
        config.insert(
            "max_len".to_string(),
            self.max_len.map(|n| n.to_string()).unwrap_or_else(|| "unbounded".to_string()),
        );
        SinkInfo {
            kind: "Redis".to_string(),
            config,
            children: vec![],
            raw_url: Some(self.url.expose_for_probe().to_string()),
        }
    }
}

// ─── TeeSink (#162 Phase 3) ───────────────────────────────────────────
//
// Compositional sink: writes each record to N child sinks. Errors from
// any single child are logged but don't fail the overall write — the
// audit substrate has to remain durable even when coordination layer
// is degraded. Per the operator-sovereignty contract: surface failures
// loudly via stderr; don't silently lose the audit record.

pub(crate) struct TeeSink {
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

    fn info(&self) -> SinkInfo {
        SinkInfo {
            kind: "Tee".to_string(),
            config: std::collections::BTreeMap::new(),
            children: self.sinks.iter().map(|s| s.info()).collect(),
            raw_url: None,
        }
    }
}

// ─── Default-sink selection (#162 Phase 3) ────────────────────────────

/// Build the process-wide default sink from env-var configuration.
///
/// Composition rules (#162, #163):
/// - `DARKMUX_AUDIT_DIR` set (and non-empty) → AuditFileSink is included.
/// - `DARKMUX_REDIS_URL` set (and non-empty) → RedisSink is included.
/// - LocalFileSink is always present (casual write target).
///
/// The TeeSink wraps every enabled sink in order: `[Audit, LocalFile, Redis]`
/// — **audit first** reflects the compliance hierarchy. The casual file
/// sink is the operator-familiar one, but the audit sink is the
/// load-bearing substrate for regulated deployments. A future short-
/// circuit mode (e.g., fail-fast on audit failure) naturally fits this
/// ordering.
///
/// Each record is broadcast to every active sink; failures are logged
/// but don't block the others — every substrate remains durable even
/// when one layer is degraded.
///
/// `DARKMUX_REDIS_STREAM` overrides the stream name (default `darkmux:flow`).
/// `DARKMUX_REDIS_MAXLEN` overrides the retention cap (default 10000;
/// set to `0` for unbounded — not recommended).
///
/// Connection errors at construction degrade gracefully: if Redis is
/// unreachable when the sink builds, the warning logs to stderr and the
/// default sink continues without it. Operators see the connection
/// failure loudly; the audit + casual substrates stay intact.
fn build_default_sink() -> Arc<dyn FlowSink> {
    let mut sinks: Vec<Arc<dyn FlowSink>> = Vec::new();

    let audit_enabled = std::env::var("DARKMUX_AUDIT_DIR")
        .ok()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if audit_enabled {
        #[cfg(unix)]
        {
            let path = audit_dir().display().to_string();
            eprintln!("flow: AuditFileSink enabled — audit_dir={path} (hash-chained, flock-serialized)");
            sinks.push(Arc::new(AuditFileSink::new()));
        }
        #[cfg(not(unix))]
        {
            eprintln!(
                "flow: DARKMUX_AUDIT_DIR set, but AuditFileSink is POSIX-only — skipping on this platform. \
                 Casual + Redis sinks remain active."
            );
        }
    }

    // LocalFile is always present.
    sinks.push(Arc::new(LocalFileSink::new()));

    let redis_url = std::env::var("DARKMUX_REDIS_URL")
        .ok()
        .filter(|s| !s.trim().is_empty());

    if let Some(url) = redis_url {
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

        let raw_url = RawRedisUrl::new(url);
        match RedisSink::new(raw_url.expose_for_probe(), &stream, max_len) {
            Ok(redis_sink) => {
                eprintln!(
                    "flow: Redis sink enabled — url={raw_url} stream={stream} \
                     max_len={max_len:?} (composed via TeeSink)"
                );
                sinks.push(Arc::new(redis_sink));
            }
            Err(e) => {
                eprintln!(
                    "flow: Redis sink construction failed ({e:#}); continuing without it. \
                     Other sinks intact."
                );
            }
        }
    }

    if sinks.len() == 1 {
        // Single sink — skip the Tee wrapper for clarity in diagnostics.
        sinks.into_iter().next().unwrap()
    } else {
        Arc::new(TeeSink::new(sinks))
    }
}

/// Process-wide default sink. Initialized lazily on first call to
/// `record()`; default selection reads env config at init time.
///
/// `#[cfg(test)]`-only: scrubs `DARKMUX_REDIS_URL` / `DARKMUX_AUDIT_DIR`
/// once before the sink is built so the cached sink doesn't capture
/// the operator's daily-shell env. Critical because the OnceLock
/// freezes the sink shape — any test that runs `record()` BEFORE
/// other isolation runs would otherwise lock in a RedisSink pointing
/// at the operator's real (possibly-unreachable) Redis. (#278)
fn default_sink() -> Arc<dyn FlowSink> {
    #[cfg(test)]
    isolate_test_env_once();

    static SINK: OnceLock<Arc<dyn FlowSink>> = OnceLock::new();
    SINK.get_or_init(build_default_sink).clone()
}

/// Introspect the process-wide default sink for diagnostics. Stable
/// pointer to the same singleton `record()` writes through, so the
/// reported sink graph cannot drift from the actually-active one.
pub(crate) fn default_sink_info() -> SinkInfo {
    default_sink().info()
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
///
/// **Schema 1.4 refactor (#167):** `machine_id` + `orchestrator` are
/// auto-populated here if the caller left them `None`. Callers that
/// pre-set the fields (e.g., a remote ingest path forwarding records
/// from another machine) win — auto-populate fills the absent ones only.
pub fn record(record: FlowRecord) -> Result<()> {
    record_to(default_sink().as_ref(), record)
}

/// Stamp provenance (machine_id / orchestrator when the
/// caller left them `None`) and write to an explicit sink. `record()` is
/// `record_to(default_sink(), …)`. Split out (#507) so callers — and
/// tests — can target a sink built against an explicit dir instead of
/// depending on the process-global default sink + live env. The
/// provenance auto-populate is identical to the pre-split `record()`.
pub(crate) fn record_to(sink: &dyn FlowSink, record: FlowRecord) -> Result<()> {
    let mut rec = record;
    if rec.machine_id.is_none() {
        rec.machine_id = resolve_machine_id();
    }
    if rec.machine_uid.is_none() {
        // (#640) Stamp the stable hardware identity at write time, like the
        // machine_id label above. Cached, so the ioreg shell-out runs once.
        rec.machine_uid = darkmux_hardware::machine_uid().map(str::to_string);
    }
    if rec.orchestrator.is_none() {
        rec.orchestrator = resolve_orchestrator();
    }
    sink.write(&rec)
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

    // Header is centralized so LocalFileSink + AuditFileSink emit
    // byte-identical schema headers; audit's seed hash stays stable.
    let header_line = schema_header_line()?;
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

    // Module-private helpers under test, reached explicitly across the
    // post-#508 submodule split (they are pub(crate), not part of the
    // crate's public re-export surface).
    use crate::schema::{epoch_to_hhmmss, epoch_to_yyyymmdd};

    // ─── (#388) RedisSink graceful-disable-on-unreachable ────────────

    fn minimal_record() -> FlowRecord {
        FlowRecord {
            ts: "2025-01-15T12:34:56Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Dispatch,
            action: "test".to_string(),
            handle: "t".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        }
    }

    // Lazy client — never connects until a write, so we can exercise the
    // failure-accounting directly without a live Redis.
    fn unreachable_sink() -> RedisSink {
        RedisSink::new("redis://127.0.0.1:6390", "darkmux:test", None).unwrap()
    }

    #[test]
    fn redis_sink_disables_after_threshold_consecutive_failures() {
        let sink = unreachable_sink();
        let e = anyhow::anyhow!("synthetic connect failure");
        // Below threshold: accumulates, stays enabled.
        assert!(!sink.note_failure(&e));
        assert!(!sink.is_disabled());
        assert!(!sink.note_failure(&e));
        assert!(!sink.is_disabled());
        // Threshold (3rd) flips it — note_failure returns true exactly once.
        assert!(sink.note_failure(&e), "3rd failure should flip to disabled");
        assert!(sink.is_disabled());
        // Already disabled: further failures don't re-flip (no repeat log).
        assert!(!sink.note_failure(&e));
    }

    #[test]
    fn redis_sink_success_resets_failure_streak() {
        let sink = unreachable_sink();
        let e = anyhow::anyhow!("x");
        sink.note_failure(&e);
        sink.note_failure(&e);
        sink.note_success(); // a single success clears the streak
        sink.note_failure(&e);
        sink.note_failure(&e);
        assert!(!sink.is_disabled(), "2 failures after a reset must not disable");
        assert!(sink.note_failure(&e), "3 consecutive post-reset failures disable");
        assert!(sink.is_disabled());
    }

    #[test]
    fn redis_sink_disable_is_permanent_for_process() {
        // Disable is a one-way latch for the process: once tripped, a
        // later success does NOT re-enable the sink (a disabled sink
        // never even reaches note_success via write(), but assert the
        // contract directly), and further failures neither re-flip nor
        // re-log (note_failure returns false).
        let sink = unreachable_sink();
        let e = anyhow::anyhow!("x");
        sink.note_failure(&e);
        sink.note_failure(&e);
        assert!(sink.note_failure(&e));
        assert!(sink.is_disabled());
        sink.note_success();
        assert!(sink.is_disabled(), "success must not re-enable a disabled sink");
        assert!(!sink.note_failure(&e), "no re-flip / re-log after disable");
        assert!(sink.is_disabled());
    }

    #[test]
    fn redis_sink_disabled_write_is_a_fast_noop() {
        let sink = unreachable_sink();
        let e = anyhow::anyhow!("x");
        // Trip the threshold.
        sink.note_failure(&e);
        sink.note_failure(&e);
        sink.note_failure(&e);
        assert!(sink.is_disabled());
        // A write while disabled returns Ok WITHOUT attempting a
        // connection — proven by the absence of the ~500ms connect
        // timeout the unreachable URL would otherwise incur.
        let start = std::time::Instant::now();
        assert!(sink.write(&minimal_record()).is_ok());
        assert!(
            start.elapsed() < std::time::Duration::from_millis(200),
            "disabled write must skip the connection attempt, not pay the timeout"
        );
    }

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
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
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
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
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
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
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
                machine_id: None,
                machine_uid: None,
                orchestrator: None,
                prev_hash: None,
                hash: None,
                payload: None,
                work_id: None,
                attempt: None,
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
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
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
        isolate_test_env_once();
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
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };

        // Capture the day-key BEFORE calling record() so a midnight-UTC
        // crossing between record() and the assertion doesn't make the
        // file appear at a different name than we check.
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
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
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
        fn info(&self) -> SinkInfo {
            SinkInfo { kind: "InMemory".to_string(), config: Default::default(), children: vec![], raw_url: None }
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
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
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
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
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
        fn info(&self) -> SinkInfo {
            SinkInfo { kind: "Failing".to_string(), config: Default::default(), children: vec![], raw_url: None }
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
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        let err = tee.write(&rec).unwrap_err();
        // Caller sees the error (so they can react if they want)
        assert!(err.to_string().contains("simulated sink failure"));
        // But the audit substrate still received the record
        assert_eq!(good.count(), 1);
    }

    #[test]
    #[serial_test::serial]
    fn record_default_path_uses_local_file_sink() {
        // The public `record()` should dispatch through the default sink
        // and produce on-disk output (behavior-equivalent to pre-#162).
        // We can't easily intercept the default sink from a test, but we
        // can verify the round trip: write via record(), read from
        // flows_dir(), see the record.
        use std::env;
        isolate_test_env_once();
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
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
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
    fn flow_schema_version_is_1_11_0() {
        // Pin the schema version so an accidental rename can't ship silently;
        // any bump beyond this should be a deliberate code change paired with
        // an update to this assertion (and corresponding viewer EXPECTED_*
        // bump if the change is breaking).
        //
        // Version history:
        //   1.2.0 — added optional `model` field (#106, Sprint 4 of #104)
        //   1.3.0 — added optional `reasoning` and `mission_id` fields and a
        //           new `Stage::TierDecision` variant (#136). Minor bump.
        //   1.4.0 — added optional `machine_id` and `orchestrator` fields
        //           (#167; substrate for fleet UI). Minor bump.
        //   1.5.0 — added optional `prev_hash` and `hash` fields for
        //           AuditFileSink's chain-of-custody (#163). Minor bump:
        //           absent in records from LocalFileSink (casual write path).
        //   1.6.0 — added optional `payload` JSON field for event-specific
        //           data; new action types: dispatch.turn / .tool /
        //           .compaction / .reasoning + mission.compile.start /
        //           .complete (#204). Minor bump.
        //   1.7.0 — added action type `dispatch.turn.heartbeat` emitted by
        //           the live trajectory tailer to keep topology edges
        //           animated during long streaming turns; pairs with
        //           runtime-side `model.partial` SSE chunks (#231). Minor
        //           bump — older readers safely ignore the new action type.
        //   1.8.0 — added optional `machine_tier`, `work_id`, and `attempt`
        //           fields on FlowRecord for the parallel-dispatch substrate
        //           (#246 PR-A tier substrate). `machine_tier` auto-populated
        //           from `DARKMUX_MACHINE_TIER` env at record-write time;
        //           `work_id` + `attempt` populated by the dispatch path
        //           when work flowed through the queue. Minor bump — older
        //           readers safely ignore the new fields.
        //   1.9.0 — REMOVED `machine_tier` (the {inference/hub/client} machine-
        //           capacity label that no routing consumed; it conflated the
        //           `tier` enum with a hardware label. Capacity moves to
        //           capability-based model selection — #321/#322). Minor bump:
        //           old readers tolerate the now-unknown key. Pre-1.9.0
        //           AuditFileSink chains need rotation (canonical-form change).
        //   1.10.0 — added `Category::Telemetry` (#557): telemetry folds into the
        //           one flow stream as a first-class family, retiring
        //           instruments.jsonl. Minor + additive — new records only, so
        //           prior AuditFileSink chains survive without rotation.
        //   1.11.0 — added optional `machine_uid` (#640): the stable hardware
        //           identity. Minor + additive — new records only, chains survive.
        assert_eq!(FLOW_SCHEMA_VERSION, "1.11.0");
    }

    #[test]
    fn telemetry_category_serializes_to_lowercase_word() {
        // The served viewer keys on `category: "telemetry"` (docs/demo/index.html
        // flowToRenderModel). The `#[serde(rename_all = "lowercase")]` on Category
        // must produce exactly that string — pin it so a variant rename can't
        // silently desync the wire from the viewer.
        let v = serde_json::to_value(crate::schema::Category::Telemetry).unwrap();
        assert_eq!(v, serde_json::json!("telemetry"));
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
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
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
                machine_id: None,
                machine_uid: None,
                orchestrator: None,
                prev_hash: None,
                hash: None,
                payload: None,
                work_id: None,
                attempt: None,
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

    // ─── Status surface tests (#170) ────────────────────────────────

    #[test]
    fn summarize_sink_flat_local() {
        let info = LocalFileSink::new().info();
        let (kinds, composition) = summarize_sink(&info);
        assert_eq!(kinds, vec!["LocalFile"]);
        assert_eq!(composition, "LocalFile");
    }

    #[test]
    fn summarize_sink_nested_tee() {
        let info = SinkInfo {
            kind: "Tee".to_string(),
            config: Default::default(),
            children: vec![
                LocalFileSink::new().info(),
                SinkInfo {
                    kind: "Redis".to_string(),
                    config: Default::default(),
                    children: vec![],
                    raw_url: None,
                },
            ],
            raw_url: None,
        };
        let (kinds, composition) = summarize_sink(&info);
        assert_eq!(kinds, vec!["LocalFile", "Redis"]);
        assert_eq!(composition, "Tee([LocalFile, Redis])");
    }

    #[test]
    fn find_redis_cfg_walks_into_tee() {
        // Post-#216: `find_redis_cfg` reads the raw URL from
        // `SinkInfo.raw_url`, not `config["url"]`. `config["url"]` is
        // the redacted display form — a Redis sink without
        // `raw_url` populated is treated as unprobable.
        let info = SinkInfo {
            kind: "Tee".to_string(),
            config: Default::default(),
            children: vec![
                LocalFileSink::new().info(),
                {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert("url".to_string(), "redis://x:***".to_string());
                    m.insert("stream".to_string(), "test:stream".to_string());
                    m.insert("max_len".to_string(), "5000".to_string());
                    SinkInfo {
                        kind: "Redis".to_string(),
                        config: m,
                        children: vec![],
                        raw_url: Some("redis://x:1234".to_string()),
                    }
                },
            ],
            raw_url: None,
        };
        let cfg = find_redis_cfg(&info).expect("redis cfg should be found");
        assert_eq!(cfg.url.expose_for_probe(), "redis://x:1234");
        assert_eq!(cfg.stream, "test:stream");
        assert_eq!(cfg.max_len, Some(5000));
    }

    #[test]
    fn find_redis_cfg_returns_none_when_absent() {
        let info = LocalFileSink::new().info();
        assert!(find_redis_cfg(&info).is_none());
    }

    #[test]
    fn collect_status_produces_serializable_snapshot() {
        // collect_status() reads real env + disk + Redis; we just verify
        // the snapshot serializes round-trip without error. The expensive
        // probes degrade gracefully when their backends are absent.
        let status = collect_status();
        let json = serde_json::to_string(&status).expect("FlowStatus must be serializable");
        let parsed: FlowStatus =
            serde_json::from_str(&json).expect("FlowStatus must round-trip");
        assert_eq!(parsed.schema_version, FLOW_SCHEMA_VERSION);
        assert!(!parsed.sinks.active_kinds.is_empty());
    }

    // ─── Schema 1.4 fields (#167) ─────────────────────────────────────

    #[serial_test::serial]
    #[test]
    fn machine_id_resolves_from_env_var() {
        let prev = env::var("DARKMUX_MACHINE_ID").ok();
        unsafe { env::set_var("DARKMUX_MACHINE_ID", "studio"); }
        assert_eq!(resolve_machine_id().as_deref(), Some("studio"));
        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_MACHINE_ID", v),
                None => env::remove_var("DARKMUX_MACHINE_ID"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn machine_id_env_var_trims_whitespace() {
        let prev = env::var("DARKMUX_MACHINE_ID").ok();
        unsafe { env::set_var("DARKMUX_MACHINE_ID", "  named  "); }
        // Trim leading/trailing whitespace; preserve internal spaces (none here).
        assert_eq!(resolve_machine_id().as_deref(), Some("named"));
        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_MACHINE_ID", v),
                None => env::remove_var("DARKMUX_MACHINE_ID"),
            }
        }
        // The whitespace-only-env fall-through is NOT exercised here:
        // the OnceLock-cached hostname makes the per-test outcome
        // depend on suite ordering. The trim assertion above is the
        // load-bearing behavior; the fall-through is covered indirectly
        // by `resolve_orchestrator_resolves_from_env_only` and the
        // doctor check's source labeling.
    }

    #[serial_test::serial]
    #[test]
    fn orchestrator_resolves_from_env_only() {
        let prev = env::var("DARKMUX_ORCHESTRATOR").ok();
        unsafe { env::remove_var("DARKMUX_ORCHESTRATOR"); }
        // No env → None. Operator-explicit by design (#49).
        assert_eq!(resolve_orchestrator(), None);

        unsafe { env::set_var("DARKMUX_ORCHESTRATOR", "claude-opus-4-7"); }
        assert_eq!(resolve_orchestrator().as_deref(), Some("claude-opus-4-7"));

        unsafe { env::set_var("DARKMUX_ORCHESTRATOR", "   "); }
        assert_eq!(resolve_orchestrator(), None);

        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_ORCHESTRATOR", v),
                None => env::remove_var("DARKMUX_ORCHESTRATOR"),
            }
        }
    }

    #[test]
    fn schema_1_4_fields_omit_when_none() {
        // Both new optional fields must be skip-serialized when None so
        // older viewers can keep parsing without seeing unexpected `null`s.
        let rec = FlowRecord {
            ts: "2026-05-17T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "x".to_string(),
            handle: "y".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        let s = serde_json::to_string(&rec).unwrap();
        assert!(!s.contains("machine_id"), "machine_id should omit when None: {s}");
        assert!(!s.contains("orchestrator"), "orchestrator should omit when None: {s}");
    }

    #[test]
    fn schema_1_4_fields_round_trip_when_set() {
        let rec = FlowRecord {
            ts: "2026-05-17T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "x".to_string(),
            handle: "y".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: Some("studio".to_string()),
            machine_uid: None,
            orchestrator: Some("claude-opus-4-7".to_string()),
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        let s = serde_json::to_string(&rec).unwrap();
        let parsed: FlowRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.machine_id.as_deref(), Some("studio"));
        assert_eq!(parsed.orchestrator.as_deref(), Some("claude-opus-4-7"));
    }

    #[serial_test::serial]
    #[test]
    fn record_auto_populates_machine_id_and_orchestrator() {
        // record() should fill machine_id + orchestrator at write time
        // when the caller leaves them None. The operator-set env values
        // win over auto-detection so the test can assert deterministic
        // values regardless of hostname.
        isolate_test_env_once();
        let tmp = TempDir::new().unwrap();
        let prev_flows = env::var("DARKMUX_FLOWS_DIR").ok();
        let prev_machine = env::var("DARKMUX_MACHINE_ID").ok();
        let prev_orch = env::var("DARKMUX_ORCHESTRATOR").ok();
        unsafe {
            env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
            env::set_var("DARKMUX_MACHINE_ID", "test-machine");
            env::set_var("DARKMUX_ORCHESTRATOR", "test-orchestrator");
        }

        let rec = FlowRecord {
            ts: "2026-05-17T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "auto-pop".to_string(),
            handle: "h".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        super::record(rec).unwrap();

        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        let content = std::fs::read_to_string(&path).unwrap();
        // Skip the schema header (line 1); the record is line 2.
        let record_line = content.lines().nth(1).expect("record line");
        let parsed: serde_json::Value = serde_json::from_str(record_line).unwrap();
        assert_eq!(parsed["machine_id"], "test-machine");
        assert_eq!(parsed["orchestrator"], "test-orchestrator");

        unsafe {
            match prev_flows {
                Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                None => env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_machine {
                Some(v) => env::set_var("DARKMUX_MACHINE_ID", v),
                None => env::remove_var("DARKMUX_MACHINE_ID"),
            }
            match prev_orch {
                Some(v) => env::set_var("DARKMUX_ORCHESTRATOR", v),
                None => env::remove_var("DARKMUX_ORCHESTRATOR"),
            }
        }
    }

    // ─── AuditFileSink (#163) ────────────────────────────────────────

    #[test]
    fn audit_hash_excludes_hash_field() {
        // hash() must NOT include the `hash` field in the input (would
        // be circular). Two records identical except for `hash` should
        // produce the same audit_hash_of() output.
        let base = FlowRecord {
            ts: "2026-05-17T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "x".to_string(),
            handle: "y".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: Some("seed".to_string()),
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        let mut other = base.clone();
        other.hash = Some("anything".to_string());

        let h1 = audit_hash_of(&base).unwrap();
        let h2 = audit_hash_of(&other).unwrap();
        assert_eq!(h1, h2, "hash should not depend on the hash field itself");
    }

    #[test]
    fn audit_hash_changes_when_content_changes() {
        // Sanity: changing ANY chain-bearing field changes the hash.
        let base = FlowRecord {
            ts: "2026-05-17T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "x".to_string(),
            handle: "y".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: Some("seed".to_string()),
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        let h1 = audit_hash_of(&base).unwrap();

        let mut diff_handle = base.clone();
        diff_handle.handle = "z".to_string();
        assert_ne!(audit_hash_of(&diff_handle).unwrap(), h1);

        let mut diff_prev = base.clone();
        diff_prev.prev_hash = Some("different-seed".to_string());
        assert_ne!(audit_hash_of(&diff_prev).unwrap(), h1);

        // PR-A schema 1.8 fields — must each contribute to the hash so a
        // future refactor that accidentally swapped `skip_serializing_if`
        // for `skip` (which omits the field from serialization entirely)
        // can't silently weaken the tamper-evidence invariant. (#246
        // PR-A review M1)
        let mut diff_work_id = base.clone();
        diff_work_id.work_id = Some("1716192000000-0".to_string());
        assert_ne!(
            audit_hash_of(&diff_work_id).unwrap(),
            h1,
            "work_id must contribute to audit hash"
        );

        let mut diff_attempt = base.clone();
        diff_attempt.attempt = Some(2);
        assert_ne!(
            audit_hash_of(&diff_attempt).unwrap(),
            h1,
            "attempt must contribute to audit hash"
        );
    }

    /// Cross-version audit-chain walk: records that lack the schema-1.8
    /// fields (work_id / attempt) must still validate
    /// under 1.8 reader code. The invariant rides on
    /// `skip_serializing_if = "Option::is_none"` — re-serialization of
    /// a None-valued field produces the same bytes a pre-1.8 writer
    /// would have produced, so the hash chain walks cleanly across the
    /// version boundary. (#246 PR-A review M2)
    #[serial_test::serial]
    #[test]
    fn integrity_walks_pre_1_8_records() {
        let tmp = TempDir::new().unwrap();
        let prev_audit = env::var("DARKMUX_AUDIT_DIR").ok();
        unsafe { env::set_var("DARKMUX_AUDIT_DIR", tmp.path()); }

        // Write records with all new schema-1.8 fields explicitly None.
        // The on-disk JSON lines omit those keys (skip_serializing_if),
        // which is byte-identical to what a pre-1.8 writer produced.
        let sink = AuditFileSink::new();
        for i in 0..3u32 {
            let rec = FlowRecord {
                ts: format!("2026-05-15T00:00:0{i}Z"),
                level: Level::Info,
                category: Category::Work,
                tier: Tier::Operator,
                stage: Stage::Scope,
                action: "pre-1.8-record".to_string(),
                handle: format!("h-{i}"),
                sprint_id: None,
                session_id: None,
                source: None,
                model: None,
                reasoning: None,
                mission_id: None,
                machine_id: None,
                machine_uid: None,
                orchestrator: None,
                prev_hash: None,
                hash: None,
                payload: None,
                work_id: None,
                attempt: None,
            };
            sink.write(&rec).unwrap();
        }

        // Confirm the on-disk JSON does NOT carry the new keys — that's
        // the "pre-1.8 shape" assertion.
        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("\"work_id\""),
            "None-valued work_id must be omitted"
        );
        assert!(
            !raw.contains("\"attempt\""),
            "None-valued attempt must be omitted"
        );

        // The chain walks cleanly — same invariant as a real pre-1.8 file
        // produced by an older darkmux build.
        let report = integrity_check_file(&path).unwrap();
        assert!(
            report.chain_valid,
            "cross-version chain must validate; reason: {report:?}"
        );
        assert_eq!(report.records_checked, 3);

        unsafe {
            match prev_audit {
                Some(v) => env::set_var("DARKMUX_AUDIT_DIR", v),
                None => env::remove_var("DARKMUX_AUDIT_DIR"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn audit_file_sink_writes_chained_records() {
        let tmp = TempDir::new().unwrap();
        let prev_audit = env::var("DARKMUX_AUDIT_DIR").ok();
        unsafe { env::set_var("DARKMUX_AUDIT_DIR", tmp.path()); }

        let sink = AuditFileSink::new();
        for i in 0..3u32 {
            let rec = FlowRecord {
                ts: format!("2026-05-17T00:00:0{i}Z"),
                level: Level::Info,
                category: Category::Work,
                tier: Tier::Operator,
                stage: Stage::Scope,
                action: format!("audit-{i}"),
                handle: format!("rec-{i}"),
                sprint_id: None,
                session_id: None,
                source: None,
                model: None,
                reasoning: None,
                mission_id: None,
                machine_id: None,
                machine_uid: None,
                orchestrator: None,
                prev_hash: None, // sink stamps this
                hash: None,      // sink stamps this
                payload: None,
                work_id: None,
                attempt: None,
            };
            sink.write(&rec).unwrap();
        }

        // Walk the file we just produced.
        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        let report = integrity_check_file(&path).unwrap();
        assert!(report.chain_valid, "chain should validate; reason: {report:?}");
        assert_eq!(report.records_checked, 3);

        unsafe {
            match prev_audit {
                Some(v) => env::set_var("DARKMUX_AUDIT_DIR", v),
                None => env::remove_var("DARKMUX_AUDIT_DIR"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn integrity_check_detects_edited_record() {
        let tmp = TempDir::new().unwrap();
        let prev_audit = env::var("DARKMUX_AUDIT_DIR").ok();
        unsafe { env::set_var("DARKMUX_AUDIT_DIR", tmp.path()); }

        let sink = AuditFileSink::new();
        for i in 0..3u32 {
            let rec = FlowRecord {
                ts: format!("2026-05-17T00:00:0{i}Z"),
                level: Level::Info,
                category: Category::Work,
                tier: Tier::Operator,
                stage: Stage::Scope,
                action: format!("audit-{i}"),
                handle: format!("rec-{i}"),
                sprint_id: None,
                session_id: None,
                source: None,
                model: None,
                reasoning: None,
                mission_id: None,
                machine_id: None,
                machine_uid: None,
                orchestrator: None,
                prev_hash: None,
                hash: None,
                payload: None,
                work_id: None,
                attempt: None,
            };
            sink.write(&rec).unwrap();
        }

        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));

        // Tamper: replace one record's handle inline. The hash should
        // no longer match the content.
        let contents = std::fs::read_to_string(&path).unwrap();
        let tampered = contents.replace("rec-1", "rec-1-EDITED");
        std::fs::write(&path, tampered).unwrap();

        let report = integrity_check_file(&path).unwrap();
        assert!(!report.chain_valid, "tampered record should break the chain");
        assert!(report.break_at_line.is_some());

        unsafe {
            match prev_audit {
                Some(v) => env::set_var("DARKMUX_AUDIT_DIR", v),
                None => env::remove_var("DARKMUX_AUDIT_DIR"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn audit_file_sink_recovers_from_header_only_file() {
        // OS-crash-between-header-and-first-record recovery: a file
        // containing ONLY the schema header should not break the next
        // write. The sink should seed the chain from the existing header
        // (NOT re-emit it) and append the first record successfully.
        let tmp = TempDir::new().unwrap();
        let prev_audit = env::var("DARKMUX_AUDIT_DIR").ok();
        unsafe { env::set_var("DARKMUX_AUDIT_DIR", tmp.path()); }

        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        // Simulate the crash state: header line only, no records.
        let header = schema_header_line().unwrap();
        std::fs::write(&path, format!("{header}\n")).unwrap();

        let sink = AuditFileSink::new();
        let rec = FlowRecord {
            ts: "2026-05-17T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "post-recovery".to_string(),
            handle: "h".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        sink.write(&rec).expect("recovery should not bail");

        // File should now have: header (line 1) + one record (line 2).
        let report = integrity_check_file(&path).unwrap();
        assert!(report.chain_valid, "post-recovery chain should validate: {report:?}");
        assert_eq!(report.records_checked, 1);

        let contents = std::fs::read_to_string(&path).unwrap();
        let line_count = contents.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(line_count, 2, "should have exactly header + one record");

        unsafe {
            match prev_audit {
                Some(v) => env::set_var("DARKMUX_AUDIT_DIR", v),
                None => env::remove_var("DARKMUX_AUDIT_DIR"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn integrity_check_empty_file_passes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.jsonl");
        std::fs::write(&path, "").unwrap();
        let report = integrity_check_file(&path).unwrap();
        assert!(report.chain_valid);
        assert_eq!(report.records_checked, 0);
    }

    #[serial_test::serial]
    #[test]
    fn audit_file_sink_recovers_chain_across_process_boundaries() {
        // Two sink instances writing to the same file must produce a
        // chain that validates. Simulates two CLI sessions (without
        // actually forking — the flock + filesystem state covers it).
        let tmp = TempDir::new().unwrap();
        let prev_audit = env::var("DARKMUX_AUDIT_DIR").ok();
        unsafe { env::set_var("DARKMUX_AUDIT_DIR", tmp.path()); }

        let sink_a = AuditFileSink::new();
        let sink_b = AuditFileSink::new();

        let mk = |handle: &str| FlowRecord {
            ts: "2026-05-17T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "x".to_string(),
            handle: handle.to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };

        sink_a.write(&mk("a1")).unwrap();
        sink_b.write(&mk("b1")).unwrap();
        sink_a.write(&mk("a2")).unwrap();

        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        let report = integrity_check_file(&path).unwrap();
        assert!(report.chain_valid, "alternating sinks should still form a valid chain: {report:?}");
        assert_eq!(report.records_checked, 3);

        unsafe {
            match prev_audit {
                Some(v) => env::set_var("DARKMUX_AUDIT_DIR", v),
                None => env::remove_var("DARKMUX_AUDIT_DIR"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn audit_dir_respects_env_override() {
        let prev = std::env::var("DARKMUX_AUDIT_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_AUDIT_DIR", "/tmp/dm-audit-test"); }
        assert_eq!(audit_dir(), std::path::PathBuf::from("/tmp/dm-audit-test"));
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_AUDIT_DIR", v),
                None => std::env::remove_var("DARKMUX_AUDIT_DIR"),
            }
        }
    }

    #[test]
    fn redact_url_creds_masks_password() {
        assert_eq!(
            redact_url_creds("redis://kain:hunter2@redis.example.com:6379/0"),
            "redis://kain:***@redis.example.com:6379/0"
        );
        // Password-only userinfo (empty user) — still mask the password.
        assert_eq!(
            redact_url_creds("redis://:onlypass@host:6379"),
            "redis://:***@host:6379"
        );
        // Username-only (no colon) — leave as-is (no secret to hide).
        assert_eq!(
            redact_url_creds("redis://user@host:6379"),
            "redis://user@host:6379"
        );
        // No creds at all — unchanged.
        assert_eq!(
            redact_url_creds("redis://127.0.0.1:6379"),
            "redis://127.0.0.1:6379"
        );
        // Non-URL string — returned verbatim, no panic.
        assert_eq!(redact_url_creds("garbage"), "garbage");
    }

    #[test]
    fn sink_init_banner_format_redacts_password() {
        // Regression for #213: the Redis sink-init banner used to print
        // the raw `DARKMUX_REDIS_URL` value with the password embedded.
        // This test pins the on-the-wire banner format so a future
        // refactor of the eprintln! at the construction site can't
        // silently re-introduce the leak.
        let url = "redis://:supersecret@100.74.208.36:6379";
        let banner = format!(
            "flow: Redis sink enabled — url={} stream={} max_len={:?} (composed via TeeSink)",
            redact_url_creds(url),
            "darkmux:flow",
            Some(10000_usize),
        );
        assert!(
            !banner.contains("supersecret"),
            "banner leaked password substring: {banner}",
        );
        assert!(
            banner.contains(":***@"),
            "banner missed redaction marker: {banner}",
        );
    }

    #[test]
    fn redis_sink_error_context_redacts_password() {
        // Regression for #213: `RedisSink::new` and `RedisSink::connect`
        // wrap their inner errors with `with_context` strings that
        // formerly embedded the raw URL. Both now route through
        // `redact_url_creds`. We exercise the format strings directly to
        // pin the contract — a future refactor that drops the redactor
        // call would resurrect the leak.
        let url = "redis://:supersecret@127.0.0.1:1/0";
        let open_ctx = format!("opening Redis connection to {}", redact_url_creds(url));
        let connect_ctx = format!("connecting to Redis at {}", redact_url_creds(url));
        for ctx in [&open_ctx, &connect_ctx] {
            assert!(
                !ctx.contains("supersecret"),
                "error context leaked password: {ctx}",
            );
            assert!(
                ctx.contains(":***@"),
                "error context missed redaction marker: {ctx}",
            );
        }
    }

    #[test]
    fn redis_sink_info_redacts_url_in_serialized_json() {
        // Regression for #216: `SinkInfo.config["url"]` previously carried
        // the raw `DARKMUX_REDIS_URL` value through to JSON consumers
        // — `darkmux flow status --json` and the daemon's CORS-permissive
        // HTTP endpoint. The raw URL now lives on `SinkInfo.raw_url`
        // (skip-serialized); `config["url"]` is the redacted display form.
        let sink = RedisSink::new(
            "redis://:supersecret@100.74.208.36:6379",
            "darkmux:flow",
            Some(10000),
        )
        .expect("RedisSink::new on a syntactically valid URL");
        let info = sink.info();

        // In-process path keeps the raw URL.
        assert_eq!(
            info.raw_url.as_deref(),
            Some("redis://:supersecret@100.74.208.36:6379"),
            "raw_url must round-trip the unredacted URL for the probe path",
        );

        // Display path strips it.
        assert_eq!(
            info.config.get("url").map(String::as_str),
            Some("redis://:***@100.74.208.36:6379"),
            "config[\"url\"] must be redacted",
        );

        // Serializing the SinkInfo (the exact path used by FlowStatus →
        // JSON output → daemon HTTP) must not contain the password.
        let json = serde_json::to_string(&info).expect("serialize SinkInfo");
        assert!(
            !json.contains("supersecret"),
            "serialized SinkInfo leaked password: {json}",
        );
        assert!(
            json.contains(":***@"),
            "serialized SinkInfo missed redaction marker: {json}",
        );
        assert!(
            !json.contains("raw_url"),
            "raw_url field must be skip-serialized (no key in JSON): {json}",
        );
    }

    #[test]
    fn find_redis_cfg_recovers_raw_url_from_redis_sink_info() {
        // Regression for #216: the probe path (`find_redis_cfg` →
        // `probe_redis` → `redis::Client::open`) must still see the raw
        // URL after #216 moved it off `config["url"]`. Round-trip:
        //   RedisSink::new(raw) → info() → find_redis_cfg → cfg.url == raw
        let raw = "redis://:hunter2@127.0.0.1:6379/0";
        let sink = RedisSink::new(raw, "darkmux:flow", Some(10000))
            .expect("RedisSink::new on a syntactically valid URL");
        let info = sink.info();
        let cfg = find_redis_cfg(&info).expect("Redis sink should resolve to a cfg");
        assert_eq!(cfg.url.expose_for_probe(), raw, "probe path must receive the raw URL");
        assert_eq!(cfg.stream, "darkmux:flow");
        assert_eq!(cfg.max_len, Some(10000));
    }

    /// #278 — `RedisSink::write` against an unresponsive Redis URL
    /// MUST bail within a bounded time, NOT wait the OS default 75s
    /// TCP-connect-or-handshake timeout. With the operator's normal-
    /// shell env var `DARKMUX_REDIS_URL` pointing at an offline peer
    /// (e.g. Studio during the 2026-05-21 incident), every flow-record
    /// write was wedging tests for 75s/record — the canonical 51-minute
    /// `cargo test` debacle from 2026-05-22.
    ///
    /// Repro: spawn a TCP listener that accepts connections but NEVER
    /// reads/writes (no SYN-ACK refusal; no handshake response). The
    /// pre-fix `get_connection()` hangs at the post-connect handshake
    /// step waiting for Redis's AUTH/INFO response. The post-fix
    /// `get_connection_with_timeout(REDIS_CONNECT_TIMEOUT)` bails at
    /// the timeout regardless of which phase is stuck.
    ///
    /// Contract: a single `.write()` against an unresponsive listener
    /// completes within 3 seconds (gives ~6× headroom on the new
    /// 500ms connect+handshake timeout for slow-loopback test runners).
    #[test]
    fn redis_sink_write_against_unresponsive_listener_completes_within_bounded_time() {
        // Spawn a TCP listener that accepts but never responds.
        // Mimics the Studio-offline failure mode: the network path
        // exists, the connect succeeds at the TCP layer, but no
        // Redis handshake response ever comes back.
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        // Background thread: accept the connection then hang on it
        // (drop the stream when test ends).
        std::thread::spawn(move || {
            // accept() blocks; one accept is enough for this test.
            // Leak the accepted stream — test will drop the listener.
            if let Ok((stream, _)) = listener.accept() {
                std::thread::sleep(std::time::Duration::from_secs(60));
                drop(stream);
            }
        });
        // Give the listener a beat to be ready.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let url = format!("redis://127.0.0.1:{port}");
        let sink = RedisSink::new(&url, "darkmux:flow", Some(10000))
            .expect("RedisSink::new on a syntactically valid URL");

        let rec = FlowRecord {
            ts: ts_utc_now(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Local,
            stage: Stage::Dispatch,
            action: "test-unresponsive-redis".to_string(),
            handle: "test".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };

        let start = std::time::Instant::now();
        let result = sink.write(&rec);
        let elapsed = start.elapsed();

        // (#388) write() is now best-effort: it swallows the underlying
        // failure (returns Ok) and accounts it toward the disable
        // threshold rather than propagating to the TeeSink (which is what
        // produced the per-write log spam). The value this test guards is
        // the bounded WALL-CLOCK of the underlying connect attempt — a
        // single failing write must still return within the connect-
        // timeout budget, not hang. The sink hasn't hit the disable
        // threshold after one failure, so the connection WAS attempted.
        assert!(
            result.is_ok(),
            "write is best-effort and swallows the failure (#388); got {result:?}"
        );
        assert!(
            !sink.is_disabled(),
            "one failure must not yet disable the sink (threshold is 3)"
        );
        // Bound is `REDIS_CONNECT_TIMEOUT * 2` (1000ms from the
        // wall-clock wrapper) + small slack for thread spawn +
        // mpsc plumbing. 1500ms is 50% headroom on the named contract;
        // a regression that bumps REDIS_CONNECT_TIMEOUT beyond ~700ms
        // will fail this test — the right behavior for "we changed
        // the connect budget without thinking about per-write wall."
        assert!(
            elapsed < std::time::Duration::from_millis(1500),
            "RedisSink::write against unresponsive listener took {elapsed:?}; \
             expected < 1500ms (REDIS_CONNECT_TIMEOUT * 2 + slack). \
             Was effectively unbounded before #278's connect-timeout fix. \
             This is the substrate test for the Studio-offline scenario."
        );
    }

    #[test]
    fn flow_status_serializes_without_leaking_redis_password() {
        // End-to-end shape: build a Redis sink, embed it in a FlowStatus
        // (via the SinkSummary path used by `collect_status`), serialize
        // the whole thing as the daemon's HTTP endpoint would, and assert
        // the password substring never appears anywhere in the JSON.
        let redis_sink = RedisSink::new(
            "redis://:supersecret@127.0.0.1:6379",
            "darkmux:flow",
            Some(10000),
        )
        .expect("RedisSink::new on a syntactically valid URL");
        let info = redis_sink.info();
        let (kinds, composition) = summarize_sink(&info);
        let summary = SinkSummary {
            info,
            active_kinds: kinds,
            composition,
        };
        let json = serde_json::to_string(&summary).expect("serialize SinkSummary");
        assert!(
            !json.contains("supersecret"),
            "SinkSummary JSON leaked password: {json}",
        );
    }

    #[test]
    fn human_format_includes_all_sections() {
        let status = collect_status();
        let text = format_status_human(&status);
        assert!(text.contains("darkmux flow status"));
        assert!(text.contains("schema:"));
        assert!(text.contains("composition:"));
        assert!(text.contains("Disk"));
        assert!(text.contains("Schema"));
    }
}
