//! Flow observability — structured JSONL records for darkmux run tracking.
//!
//! # Storage model
//!
//! Records are appended to a per-day JSONL file (`YYYY-MM-DD.jsonl`) under
//! `~/.darkmux/flows/` (overridable via `DARKMUX_FLOWS_DIR`). The first write
//! atomically prepends a schema header so partial-file recovery is possible.

pub mod daemon_probe;

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const FLOW_SCHEMA_VERSION: &str = "1.8.0";
// Version history:
//   1.2.0 — added optional `model` (#106)
//   1.3.0 — added optional `reasoning` + `mission_id`; new Stage::TierDecision (#136)
//   1.4.0 — added optional `machine_id` + `orchestrator` (#167; substrate for #162 fleet UI)
//   1.5.0 — added optional `prev_hash` + `hash` (#163; AuditFileSink chain-of-custody fields)
//   1.6.0 — added optional `payload` JSON blob for event-specific fields. New action
//           values for richer dispatch observability: `dispatch.turn`, `dispatch.tool`,
//           `dispatch.compaction`, `dispatch.reasoning`, `mission.compile.start`,
//           `mission.compile.complete`. Existing `dispatch.start/complete` carry
//           runtime metadata in `payload` (runtime_path, prompt_chars, total_turns, etc.).
//           Backward-compatible — older readers ignore the new field + new actions. (#204)
//   1.7.0 — added action `dispatch.turn.heartbeat` emitted by the live trajectory
//           tailer (`crew/dispatch_internal.rs`) from streaming `model.partial` SSE
//           chunks at most once per 2s. Keeps topology edges animated mid-turn and
//           closes the post-exit-only observability gap. Backward-compatible —
//           older readers safely ignore the new action. (#231)
//   1.8.0 — added optional `machine_tier` (the hardware tier of the emitting machine —
//           `"inference"` / `"hub"` / `"client"`), `work_id` (the work-queue claim id
//           for parallel-dispatch jobs), and `attempt` (retry counter; 1 = first try)
//           for the Article 4 parallel-dispatch substrate (#246). `machine_tier` is
//           auto-populated from `DARKMUX_MACHINE_TIER` env at record-write time, same
//           pattern as `machine_id`. `work_id` and `attempt` are populated by the
//           dispatch path when the work flowed through the queue; absent on direct
//           local dispatches. Backward-compatible — older readers ignore the new
//           fields. (#246 PR-A tier substrate)

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
    /// Machine that emitted this record. Auto-populated at write time
    /// from `DARKMUX_MACHINE_ID` env (operator-named — e.g. `"studio"`,
    /// `"mini-1"`) or hostname (default). Older records (pre-1.4.0) lack
    /// the field; viewer treats absence as `unknown`. Schema 1.4 addition
    /// (#167; substrate for fleet UI).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    /// Frontier orchestrator driving this record's session — e.g.,
    /// `"claude-opus-4-7"`, `"cursor-anthropic"`. Auto-populated from
    /// `DARKMUX_ORCHESTRATOR` env at write time. Operator-explicit by
    /// design: there's no reliable way to auto-detect the frontier-tier
    /// AI from inside darkmux. None when the operator hasn't declared.
    /// Schema 1.4 addition (#167).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<String>,
    /// BLAKE3 hash of the previous record in this audit file's chain.
    /// `None` on records written through LocalFileSink (the casual sink);
    /// AuditFileSink (the compliance-strength sibling) populates this
    /// with the prior record's `hash` value so tampering with any single
    /// record is detectable via a linear walk. The first record in a
    /// file points to the hash of the schema-header line. Schema 1.5
    /// addition (#163).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_hash: Option<String>,
    /// BLAKE3 hash of THIS record's content (excluding the `hash` field
    /// itself — see `audit_hash_of()`). Populated only by AuditFileSink.
    /// Together with `prev_hash` forms a tamper-evident chain. The
    /// `darkmux flow integrity-check` verb recomputes the chain and
    /// reports the first divergence. Schema 1.5 addition (#163).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Event-specific structured fields that aren't promoted to first-class
    /// `FlowRecord` members. Schema 1.6 addition (#204) — gives new event
    /// types (`dispatch.turn`, `dispatch.tool`, `dispatch.compaction`,
    /// `dispatch.reasoning`, `mission.compile.start/complete`) a place to
    /// carry their event-specific fields without growing the struct
    /// indefinitely.
    ///
    /// Convention: keys are snake_case strings; values are typed by event
    /// shape (e.g. `dispatch.tool` uses `tool_name: string`, `args_chars:
    /// integer`, `result_chars: integer`, `success: boolean`). See the
    /// emit sites in `dispatch.rs` / `dispatch_internal.rs` /
    /// `mission_propose.rs` for the per-event-type payload shapes.
    ///
    /// Older records (pre-1.6) lack the field; viewer treats absence as
    /// the empty object `{}`. New event types degrade to "action only" on
    /// older viewers — they see the action string and the standard
    /// FlowRecord fields, just not the event-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// Hardware tier of the machine that emitted this record — one of
    /// `"inference"` (heavy-model peer), `"hub"` (always-on infrastructure
    /// for admin agents), `"client"` (UI-only). Auto-populated at record
    /// write time from `DARKMUX_MACHINE_TIER` env, same pattern as
    /// `machine_id`. None when the operator hasn't declared a tier — the
    /// fleet topology still works for single-machine setups but tier-aware
    /// routing will bail loud. Schema 1.8 addition (#246).
    ///
    /// Distinct from the existing `tier: Tier` enum at the top of this
    /// struct: that field classifies the *record* (local-vs-frontier-vs-
    /// audit), while `machine_tier` classifies the *machine* (capacity
    /// class). Both can be set independently on the same record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_tier: Option<String>,
    /// Work-queue claim id when this record was produced by a job that
    /// flowed through `darkmux:work:<tier>`. Absent on direct local
    /// dispatches (the operator ran `darkmux crew dispatch <role>` on the
    /// local machine and tier matched). Populated by the dispatch path
    /// when it claims work from the queue. Schema 1.8 addition (#246).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
    /// Retry counter for queued work — 1 on first attempt, 2+ on retries
    /// after lease expiry. Surfaces in `darkmux doctor` as a "recent
    /// retries" rollup. Absent on direct local dispatches (no retry
    /// semantics outside the queue). Schema 1.8 addition (#246).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

/// Resolve the flows directory from env override (`DARKMUX_FLOWS_DIR`) or
/// default (`~/.darkmux/flows/`). Falls back to `/tmp/darkmux/flows/` if
/// neither is resolvable (CI / sandboxed environments without HOME).
pub fn flows_dir() -> PathBuf {
    std::env::var("DARKMUX_FLOWS_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".darkmux").join("flows")))
        .unwrap_or_else(|| PathBuf::from("/tmp/darkmux/flows"))
}

/// ISO 8601 UTC date string from current time — `YYYY-MM-DD`. Used for
/// per-day file naming (one JSONL file per UTC day), NOT for record `ts`.
pub fn day_utc_now() -> String {
    let secs = current_epoch_secs();
    let (y, m, d) = epoch_to_yyyymmdd(secs);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// ISO 8601 UTC datetime string from current time — `YYYY-MM-DDTHH:MM:SSZ`.
/// Used for `FlowRecord.ts`. Seconds precision is sufficient for the
/// dispatch / sprint timing surfaces; finer precision is a future bump.
pub fn ts_utc_now() -> String {
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

/// Compute the BLAKE3 hash of a record's canonical form. The `hash` field
/// is intentionally excluded (cloning the record and setting `hash =
/// None` before serializing) so the chain doesn't self-reference. The
/// `prev_hash` field IS included — that's what binds each record to the
/// chain.
pub fn audit_hash_of(record: &FlowRecord) -> Result<String> {
    let mut to_hash = record.clone();
    to_hash.hash = None;
    let bytes = serde_json::to_vec(&to_hash).context("serializing record for hash")?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// Hash of the schema-header line — the chain's deterministic seed. Used
/// as `prev_hash` for the first record in a fresh audit file so the
/// chain starts with a well-defined value rather than `None`.
fn audit_seed_hash(header_line: &str) -> String {
    blake3::hash(header_line.as_bytes()).to_hex().to_string()
}

/// Append `record` to the audit file at `path`, populating `prev_hash`
/// and `hash` from the existing chain. Cross-process safe via `flock(2)`
/// so concurrent CLI sessions writing the same file serialize correctly.
/// POSIX-only.
///
/// Atomicity model:
///
///   1. Acquire exclusive flock on the file (creating it if absent).
///   2. Read the last record (or the schema header for an empty file)
///      to recover the chain's current tail hash.
///   3. Populate `prev_hash` + recompute `hash` on a clone of the input.
///   4. Append the line.
///   5. Drop the file → flock released.
///
/// First-write-into-new-file emits the schema header AND the first
/// record under the same lock so an interrupt can't leave a header-only
/// file with no chain seed visible.
#[cfg(unix)]
fn audit_record_at(record: &FlowRecord, path: &Path) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating audit dir {}", parent.display()))?;
    }

    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening audit log {}", path.display()))?;

    // Acquire exclusive cross-process lock; auto-released on file drop.
    let fd = file.as_raw_fd();
    let lock_ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
    if lock_ret != 0 {
        return Err(anyhow::anyhow!(
            "flock(LOCK_EX) failed on audit log {}: errno {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    // RAII guard so the lock is released even if the function bails.
    struct FlockGuard(std::os::unix::io::RawFd);
    impl Drop for FlockGuard {
        fn drop(&mut self) {
            unsafe { libc::flock(self.0, libc::LOCK_UN) };
        }
    }
    let _guard = FlockGuard(fd);

    use std::io::{Read, Seek, SeekFrom, Write as _};
    let mut contents = String::new();
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seek to start of {}", path.display()))?;
    file.read_to_string(&mut contents)
        .with_context(|| format!("reading audit log {}", path.display()))?;

    let (prev_hash, write_header) = if contents.is_empty() {
        // Fresh file — the seed hash binds the chain to the schema header
        // we're about to write.
        let header = schema_header_line()?;
        let seed = audit_seed_hash(&header);
        (seed, Some(header))
    } else {
        // Existing file — find the last non-empty line.
        let non_empty: Vec<&str> =
            contents.lines().filter(|l| !l.trim().is_empty()).collect();
        if non_empty.is_empty() {
            // File exists but trims to nothing (whitespace-only) — treat as fresh.
            let header = schema_header_line()?;
            (audit_seed_hash(&header), Some(header))
        } else {
            let last_line = *non_empty.last().expect("non_empty is not empty per check above");
            // Parse the last line. Unparseable = chain corrupted.
            let parsed: serde_json::Value = serde_json::from_str(last_line).map_err(|e| {
                anyhow::anyhow!(
                    "audit log {} last line is unparseable JSON: {e}",
                    path.display()
                )
            })?;
            let last_hash = match parsed.get("hash").and_then(|h| h.as_str()) {
                Some(h) => h.to_string(),
                None => {
                    // No `hash` field on the last line. Two cases:
                    //   (a) File contains ONLY the schema header (process
                    //       or OS crash between header write and first-
                    //       record write — the within-process atomicity
                    //       comment above only protects same-process
                    //       interrupts). Recover by re-seeding from the
                    //       existing header so we don't double-write it.
                    //   (b) Audit log has been edited to remove hash
                    //       fields, or a non-audit JSONL was placed here.
                    //       Chain cannot continue — bail loudly.
                    if non_empty.len() == 1 {
                        audit_seed_hash(last_line)
                    } else {
                        return Err(anyhow::anyhow!(
                            "audit log {} last line lacks `hash` field — chain corrupted",
                            path.display()
                        ));
                    }
                }
            };
            (last_hash, None)
        }
    };

    // Build the record to write: stamp prev_hash, recompute hash.
    let mut to_write = record.clone();
    to_write.prev_hash = Some(prev_hash);
    to_write.hash = None;
    let hash = audit_hash_of(&to_write).context("computing audit hash")?;
    to_write.hash = Some(hash);

    let line = serde_json::to_string(&to_write).context("serializing audit record")?;

    // Append (after seeking to end). flock holds; PIPE_BUF guarantee is
    // belt-and-suspenders for the JSONL line.
    file.seek(SeekFrom::End(0))
        .with_context(|| format!("seek to end of {}", path.display()))?;
    if let Some(header) = write_header {
        file.write_all(header.as_bytes())
            .with_context(|| format!("writing schema header to {}", path.display()))?;
        file.write_all(b"\n")?;
    }
    file.write_all(line.as_bytes())
        .with_context(|| format!("appending record to audit log {}", path.display()))?;
    file.write_all(b"\n")?;
    file.sync_all()
        .with_context(|| format!("syncing audit log {}", path.display()))?;
    Ok(())
}

/// Build the schema header line used by both LocalFileSink (via
/// `record_at`) and AuditFileSink. Centralized so the two sinks emit
/// byte-identical headers — the audit seed hash is then stable across
/// sink kinds, and any future reader can recognize the line via
/// `_type: "schema"`.
fn schema_header_line() -> Result<String> {
    let header = serde_json::json!({
        "_type": "schema",
        "version": FLOW_SCHEMA_VERSION,
        "darkmux_version": env!("CARGO_PKG_VERSION"),
    });
    serde_json::to_string(&header).context("serializing schema header")
}

/// Walk a single audit file, recomputing the hash chain and reporting
/// the first divergence (if any). Cheap — sequential read + per-line
/// hash; throughput limited by disk read.
pub fn integrity_check_file(path: &Path) -> Result<IntegrityReport> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("reading audit log {}", path.display()))?;
    let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();

    if lines.is_empty() {
        return Ok(IntegrityReport {
            path: path.display().to_string(),
            records_checked: 0,
            chain_valid: true,
            break_at_line: None,
            break_reason: None,
        });
    }

    // Line 1 is the schema header (no hash); seed the expected prev_hash
    // from its hash so the first record's `prev_hash` should equal it.
    let header_line = lines[0];
    let mut expected_prev = audit_seed_hash(header_line);
    let mut records_checked = 0u64;

    for (idx, line) in lines.iter().enumerate().skip(1) {
        records_checked += 1;
        let rec: FlowRecord = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                return Ok(IntegrityReport {
                    path: path.display().to_string(),
                    records_checked,
                    chain_valid: false,
                    break_at_line: Some((idx + 1) as u64), // 1-indexed
                    break_reason: Some(format!("unparseable JSON: {e}")),
                });
            }
        };

        let stored_prev = rec.prev_hash.clone().unwrap_or_default();
        if stored_prev != expected_prev {
            return Ok(IntegrityReport {
                path: path.display().to_string(),
                records_checked,
                chain_valid: false,
                break_at_line: Some((idx + 1) as u64),
                break_reason: Some(format!(
                    "prev_hash mismatch: stored `{stored_prev}` != expected `{expected_prev}` (audit log has been edited or a write was interleaved)"
                )),
            });
        }

        let stored_hash = match rec.hash.clone() {
            Some(h) => h,
            None => {
                return Ok(IntegrityReport {
                    path: path.display().to_string(),
                    records_checked,
                    chain_valid: false,
                    break_at_line: Some((idx + 1) as u64),
                    break_reason: Some(
                        "record lacks `hash` field — not produced by AuditFileSink, or chain is corrupted".to_string(),
                    ),
                });
            }
        };

        let recomputed = audit_hash_of(&rec).context("recomputing audit hash")?;
        if recomputed != stored_hash {
            return Ok(IntegrityReport {
                path: path.display().to_string(),
                records_checked,
                chain_valid: false,
                break_at_line: Some((idx + 1) as u64),
                break_reason: Some(format!(
                    "hash mismatch: stored `{stored_hash}` != recomputed `{recomputed}` (record content has been edited)"
                )),
            });
        }

        expected_prev = stored_hash;
    }

    Ok(IntegrityReport {
        path: path.display().to_string(),
        records_checked,
        chain_valid: true,
        break_at_line: None,
        break_reason: None,
    })
}

/// Walk every audit file under `audit_dir()`. Sorted by filename for
/// stable output.
pub fn integrity_check_all() -> Result<Vec<IntegrityReport>> {
    let dir = audit_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(vec![]), // missing dir = nothing to check
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e == "jsonl")
                .unwrap_or(false)
        })
        .collect();
    paths.sort();
    let mut reports = Vec::with_capacity(paths.len());
    for p in paths {
        reports.push(integrity_check_file(&p)?);
    }
    Ok(reports)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityReport {
    pub path: String,
    pub records_checked: u64,
    pub chain_valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub break_at_line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub break_reason: Option<String>,
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
}

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
fn open_redis_connection_bounded(
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
        })
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

    fn info(&self) -> SinkInfo {
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
pub fn default_sink_info() -> SinkInfo {
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

/// Stamp provenance (machine_id / orchestrator / machine_tier when the
/// caller left them `None`) and write to an explicit sink. `record()` is
/// `record_to(default_sink(), …)`. Split out (#507) so callers — and
/// tests — can target a sink built against an explicit dir instead of
/// depending on the process-global default sink + live env. The
/// provenance auto-populate is identical to the pre-split `record()`.
pub fn record_to(sink: &dyn FlowSink, record: FlowRecord) -> Result<()> {
    let mut rec = record;
    if rec.machine_id.is_none() {
        rec.machine_id = resolve_machine_id();
    }
    if rec.orchestrator.is_none() {
        rec.orchestrator = resolve_orchestrator();
    }
    if rec.machine_tier.is_none() {
        rec.machine_tier = resolve_machine_tier();
    }
    sink.write(&rec)
}

/// Resolve the machine identifier for new flow records.
///
/// Order of precedence:
/// 1. `DARKMUX_MACHINE_ID` env var — operator-named (e.g. `"studio"`,
///    `"mini-1"`). Fleet operators prefer logical names over DNS-style
///    identifiers, so the env override always wins. Re-read on every
///    call so a `set_var` in tests + operator shells takes effect
///    without a process restart.
/// 2. Cached `hostname(1)` output — POSIX-portable; works on macOS,
///    Linux, BSD without adding a dep. Hostname doesn't change during
///    process lifetime, so we cache the subprocess result to keep the
///    per-record write hot-path cheap AND to avoid the thread-yield
///    that would otherwise turn `flow::record()` into a synchronization
///    hazard for tests that mutate env without `#[serial_test::serial]`.
/// 3. `None` — extremely rare (CI in a sandbox without `hostname`).
pub fn resolve_machine_id() -> Option<String> {
    if let Ok(s) = std::env::var("DARKMUX_MACHINE_ID") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    static HOSTNAME: OnceLock<Option<String>> = OnceLock::new();
    HOSTNAME
        .get_or_init(|| {
            std::process::Command::new("hostname").output().ok().and_then(|out| {
                if !out.status.success() {
                    return None;
                }
                let h = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if h.is_empty() { None } else { Some(h) }
            })
        })
        .clone()
}

/// Resolve the orchestrator identifier for new flow records.
///
/// **Operator-explicit by design** — there's no reliable way to detect
/// the frontier-tier AI driving the operator's session from inside
/// darkmux. The operator declares it via `DARKMUX_ORCHESTRATOR`; absent
/// declaration, records carry no orchestrator field and the doctor
/// surfaces a warn so the operator knows the field exists.
pub fn resolve_orchestrator() -> Option<String> {
    std::env::var("DARKMUX_ORCHESTRATOR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve the machine's hardware-tier declaration for new flow records.
///
/// **Operator-explicit by design** — same posture as `DARKMUX_ORCHESTRATOR`:
/// there's no reliable way to auto-detect whether a machine should serve
/// as an inference peer, hub, or client from inside darkmux. The operator
/// declares it via `DARKMUX_MACHINE_TIER`; absent declaration, records
/// carry no `machine_tier` field and tier-aware routing bails loud at the
/// dispatch path rather than silently substituting. (#246)
///
/// Valid values are `"inference"`, `"hub"`, `"client"`. Other values are
/// passed through unchanged (forward-compat with future tier names); the
/// dispatch path validates membership at the moment a routing decision
/// would consume the value.
pub fn resolve_machine_tier() -> Option<String> {
    std::env::var("DARKMUX_MACHINE_TIER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
pub fn record_at(record: &FlowRecord, path: &Path) -> Result<()> {
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

// ─── Status surface (#170) ────────────────────────────────────────────
//
// `darkmux flow status` and the doctor's `flow-sink-health` check both
// read from `collect_status()`. The single collector ensures the CLI
// surface and the doctor never drift — same probes, same data shape.
//
// Side effects: opens a Redis connection when Redis is configured (so
// the operator gets accurate reachability + XLEN data). Disk probes are
// read-only file I/O. No record writes.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowStatus {
    pub schema_version: String,
    pub sinks: SinkSummary,
    /// Present when Redis is configured (via `DARKMUX_REDIS_URL` env
    /// or appearing in the sink graph); `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redis: Option<RedisStatus>,
    pub disk: DiskStatus,
    pub schema: SchemaSkew,
    pub overall_state: HealthState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warn_reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fail_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SinkSummary {
    pub info: SinkInfo,
    /// Flat list of active leaf sink kinds — e.g., `["LocalFile", "Redis"]`.
    pub active_kinds: Vec<String>,
    /// Human-readable composition string — e.g., `Tee([LocalFile, Redis])`.
    pub composition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisStatus {
    pub url: String,
    pub stream: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_len: Option<usize>,
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reachability_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xlen: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_probe_ms: Option<u128>,
    /// True when XLEN is within 5% of MAXLEN — warns the operator the
    /// stream is about to start trimming old records.
    pub near_max_len: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskStatus {
    pub flows_dir: String,
    pub exists: bool,
    pub day_files: u64,
    pub total_bytes: u64,
    /// Distinct schema versions observed in day files (header line of
    /// each `YYYY-MM-DD.jsonl`). Skew detection cross-references this
    /// with `SchemaSkew.observed_versions` (which probes Redis).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_disk_schemas: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSkew {
    pub writer_version: String,
    /// Distinct schema strings observed in the active Redis stream
    /// (best-effort XREVRANGE of the last N entries). Empty when no
    /// Redis is configured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_versions: Vec<String>,
    pub skew_detected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skew_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HealthState {
    Ok,
    Warn,
    Fail,
}

/// Build a status snapshot. Cheap: ~10ms when Redis is reachable, sub-ms
/// when it isn't. Safe to call from CLI + doctor + daemon endpoint without
/// concern for throughput — the result is meant to be consumed by humans
/// or by a polling UI (every 30s+).
pub fn collect_status() -> FlowStatus {
    let info = default_sink_info();
    let (active_kinds, composition) = summarize_sink(&info);
    let redis_cfg = find_redis_cfg(&info);

    let (redis, redis_observed) = if let Some(cfg) = redis_cfg.clone() {
        let (status, observed) = probe_redis(&cfg);
        (Some(status), observed)
    } else {
        (None, vec![])
    };

    let disk = probe_disk();

    let mut warn_reasons = Vec::new();
    let mut fail_reasons = Vec::new();

    // Skew detection: ONLY Redis-observed schemas count as "live writers".
    // Disk-header schemas from older day files are historical artifacts of
    // earlier writer versions and SHOULD NOT trigger skew warnings on every
    // run — that would mean every operator who's been on darkmux >1 schema
    // bump sees a permanent warn. The Redis stream, by contrast, reflects
    // currently-active writers in the fleet.
    //
    // The disk-schemas data is still surfaced (in DiskStatus.observed_disk_schemas
    // and SchemaSkew.observed_versions) for diagnostic transparency, but
    // doesn't gate the warn_reasons rollup.
    let mut all_versions: Vec<String> = disk
        .observed_disk_schemas
        .iter()
        .chain(redis_observed.iter())
        .cloned()
        .collect();
    all_versions.sort();
    all_versions.dedup();
    let live_foreign: Vec<String> = redis_observed
        .iter()
        .filter(|v| v.as_str() != FLOW_SCHEMA_VERSION)
        .cloned()
        .collect();
    let skew_detected = !live_foreign.is_empty();
    let skew_reason = if skew_detected {
        Some(format!(
            "writer is {} but live Redis stream shows {} — at least one other writer in the fleet is on a different schema",
            FLOW_SCHEMA_VERSION,
            live_foreign.join(", ")
        ))
    } else {
        None
    };
    if skew_detected {
        warn_reasons.push("schema_skew_detected".to_string());
    }

    if let Some(r) = redis.as_ref() {
        if !r.reachable {
            warn_reasons.push("redis_unreachable".to_string());
        }
        if r.near_max_len {
            warn_reasons.push("redis_stream_near_maxlen".to_string());
        }
    }

    if !disk.exists {
        // Disk dir absent isn't fatal — first-write creates it — but the
        // operator should know they have no flows yet.
        warn_reasons.push("flows_dir_absent".to_string());
    }

    // Total sink unreachability: no active sinks (shouldn't happen — at
    // minimum LocalFile is always available — but guard anyway).
    if active_kinds.is_empty() {
        fail_reasons.push("no_active_sinks".to_string());
    }

    let overall_state = if !fail_reasons.is_empty() {
        HealthState::Fail
    } else if !warn_reasons.is_empty() {
        HealthState::Warn
    } else {
        HealthState::Ok
    };

    FlowStatus {
        schema_version: FLOW_SCHEMA_VERSION.to_string(),
        sinks: SinkSummary { info, active_kinds, composition },
        redis,
        disk,
        schema: SchemaSkew {
            writer_version: FLOW_SCHEMA_VERSION.to_string(),
            observed_versions: all_versions,
            skew_detected,
            skew_reason,
        },
        overall_state,
        warn_reasons,
        fail_reasons,
    }
}

/// Flat list of leaf kinds + composition string for a sink tree.
fn summarize_sink(info: &SinkInfo) -> (Vec<String>, String) {
    fn walk_kinds(info: &SinkInfo, out: &mut Vec<String>) {
        if info.children.is_empty() {
            out.push(info.kind.to_string());
        } else {
            for child in &info.children {
                walk_kinds(child, out);
            }
        }
    }
    fn walk_composition(info: &SinkInfo) -> String {
        if info.children.is_empty() {
            info.kind.to_string()
        } else {
            let inner: Vec<String> = info.children.iter().map(walk_composition).collect();
            format!("{}([{}])", info.kind, inner.join(", "))
        }
    }
    let mut kinds = Vec::new();
    walk_kinds(info, &mut kinds);
    (kinds, walk_composition(info))
}

/// Redis config extracted from a SinkInfo tree.
#[derive(Debug, Clone)]
struct RedisCfg {
    url: RawRedisUrl,
    stream: String,
    max_len: Option<usize>,
}

fn find_redis_cfg(info: &SinkInfo) -> Option<RedisCfg> {
    if info.kind == "Redis" {
        // The raw URL — needed for `redis::Client::open` in `probe_redis`
        // — lives on `SinkInfo.raw_url`, NOT `config["url"]`. The latter
        // is the redacted display form. A Redis sink without a populated
        // `raw_url` is unusable for probing, so treat it as absent. (#216)
        let raw_url = info.raw_url.clone()?;
        return Some(RedisCfg {
            url: RawRedisUrl::new(raw_url),
            stream: info.config.get("stream").cloned().unwrap_or_default(),
            max_len: info
                .config
                .get("max_len")
                .and_then(|s| s.parse::<usize>().ok()),
        });
    }
    info.children.iter().find_map(find_redis_cfg)
}

/// Redact `:password@` in a Redis URL for diagnostic display. Operators
/// who put credentials in `DARKMUX_REDIS_URL` shouldn't have those creds
/// echoed back through `darkmux flow status` (which is exposed via the
/// daemon's permissive-CORS endpoint and shown in the browser modal).
/// (#170 QA Q7)
///
/// Conservative: anything between the scheme and the host that contains
/// `@` is treated as `<userinfo>@`; the password portion (after the first
/// `:` in userinfo) is replaced with `***`. URLs without an `@` are
/// returned unchanged.
pub fn redact_url_creds(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let Some((userinfo, host)) = rest.split_once('@') else {
        return url.to_string();
    };
    let masked_userinfo = if let Some((user, _pass)) = userinfo.split_once(':') {
        format!("{user}:***")
    } else {
        // username only, no password — still keep the username visible.
        userinfo.to_string()
    };
    format!("{scheme}://{masked_userinfo}@{host}")
}

/// Probe Redis: open a connection, run XLEN + XREVRANGE for oldest/newest,
/// time the round-trip. Returns the status + the list of distinct schema
/// strings observed in the last 100 entries (for skew detection).
fn probe_redis(cfg: &RedisCfg) -> (RedisStatus, Vec<String>) {
    let start = std::time::Instant::now();
    let client = match redis::Client::open(cfg.url.expose_for_probe()) {
        Ok(c) => c,
        Err(e) => {
            return (
                RedisStatus {
                    url: cfg.url.to_string(),
                    stream: cfg.stream.clone(),
                    max_len: cfg.max_len,
                    reachable: false,
                    reachability_error: Some(format!("client open: {e}")),
                    xlen: None,
                    oldest_ts: None,
                    newest_ts: None,
                    last_probe_ms: None,
                    near_max_len: false,
                },
                vec![],
            );
        }
    };

    // Bounded by REDIS_CONNECT_TIMEOUT (#278) — a silent-at-TCP-layer
    // OR accept-but-don't-respond peer must not wedge the doctor.
    // Uses the wall-clock-bounded wrapper, not just redis-rs's TCP-
    // connect timeout (which doesn't cover the post-connect handshake
    // hang the Studio-offline scenario can trigger).
    let mut conn = match open_redis_connection_bounded(&client, REDIS_CONNECT_TIMEOUT) {
        Ok(c) => c,
        Err(e) => {
            return (
                RedisStatus {
                    url: cfg.url.to_string(),
                    stream: cfg.stream.clone(),
                    max_len: cfg.max_len,
                    reachable: false,
                    reachability_error: Some(format!("connect: {e}")),
                    xlen: None,
                    oldest_ts: None,
                    newest_ts: None,
                    last_probe_ms: None,
                    near_max_len: false,
                },
                vec![],
            );
        }
    };

    let xlen_res: redis::RedisResult<u64> = redis::cmd("XLEN").arg(&cfg.stream).query(&mut conn);
    let xlen = xlen_res.ok();

    // XINFO STREAM <key> would give first-entry / last-entry IDs in one
    // shot, but parsing its mixed-array response across redis-rs versions
    // is fragile. XRANGE/XREVRANGE with COUNT 1 is unambiguous.
    let oldest_id: Option<String> = redis::cmd("XRANGE")
        .arg(&cfg.stream)
        .arg("-")
        .arg("+")
        .arg("COUNT")
        .arg(1)
        .query::<Vec<(String, Vec<(String, String)>)>>(&mut conn)
        .ok()
        .and_then(|v| v.into_iter().next().map(|(id, _)| id));
    let (newest_id, schemas) = redis::cmd("XREVRANGE")
        .arg(&cfg.stream)
        .arg("+")
        .arg("-")
        .arg("COUNT")
        .arg(100)
        .query::<Vec<(String, Vec<(String, String)>)>>(&mut conn)
        .map(|entries| {
            let newest = entries.first().map(|(id, _)| id.clone());
            let schemas: Vec<String> = entries
                .iter()
                .filter_map(|(_, fields)| {
                    fields
                        .iter()
                        .find(|(k, _)| k == "schema")
                        .map(|(_, v)| v.clone())
                })
                .collect();
            (newest, schemas)
        })
        .unwrap_or((None, vec![]));

    let mut observed = schemas;
    observed.sort();
    observed.dedup();

    let last_probe_ms = start.elapsed().as_millis();

    let near_max_len = match (cfg.max_len, xlen) {
        (Some(cap), Some(len)) if cap > 0 => (len as f64) / (cap as f64) >= 0.95,
        _ => false,
    };

    (
        RedisStatus {
            url: cfg.url.to_string(),
            stream: cfg.stream.clone(),
            max_len: cfg.max_len,
            reachable: true,
            reachability_error: None,
            xlen,
            oldest_ts: oldest_id,
            newest_ts: newest_id,
            last_probe_ms: Some(last_probe_ms),
            near_max_len,
        },
        observed,
    )
}

/// Probe disk: count day files in flows_dir, sum sizes, gather header
/// schema versions for skew detection.
fn probe_disk() -> DiskStatus {
    let dir = flows_dir();
    let dir_str = dir.display().to_string();

    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => {
            return DiskStatus {
                flows_dir: dir_str,
                exists: false,
                day_files: 0,
                total_bytes: 0,
                observed_disk_schemas: vec![],
            };
        }
    };

    let mut day_files = 0u64;
    let mut total_bytes = 0u64;
    let mut schemas: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        // YYYY-MM-DD.jsonl naming convention.
        if !name.ends_with(".jsonl") || name.len() < 16 {
            continue;
        }
        day_files += 1;
        if let Ok(meta) = entry.metadata() {
            total_bytes += meta.len();
        }
        // Read just the first line (schema header) without slurping the
        // whole file. Capped at 64 KiB to guard against a corrupted
        // newline-free file forcing an unbounded read — the actual schema
        // header is ~80 bytes (#170 QA S3).
        if let Ok(file) = fs::File::open(&path) {
            use std::io::{BufRead, BufReader, Read};
            let mut reader = BufReader::new(file.take(64 * 1024));
            let mut first = String::new();
            if reader.read_line(&mut first).is_ok() {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(first.trim()) {
                    if let Some(v) = val.get("version").and_then(|v| v.as_str()) {
                        schemas.push(v.to_string());
                    }
                }
            }
        }
    }

    schemas.sort();
    schemas.dedup();

    DiskStatus {
        flows_dir: dir_str,
        exists: true,
        day_files,
        total_bytes,
        observed_disk_schemas: schemas,
    }
}

/// Human-readable rendering of a `FlowStatus`. The CLI's default
/// (non-`--json`) output.
pub fn format_status_human(status: &FlowStatus) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let state_marker = match status.overall_state {
        HealthState::Ok => "✓ ok",
        HealthState::Warn => "⚠ warn",
        HealthState::Fail => "✗ fail",
    };
    let _ = writeln!(out, "darkmux flow status — {state_marker}");
    let _ = writeln!(out, "  schema:       {}", status.schema_version);
    let _ = writeln!(out, "  composition:  {}", status.sinks.composition);

    if let Some(r) = status.redis.as_ref() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Redis");
        let _ = writeln!(out, "  url:          {}", r.url);
        let _ = writeln!(out, "  stream:       {}", r.stream);
        let _ = writeln!(
            out,
            "  max_len:      {}",
            r.max_len.map(|n| n.to_string()).unwrap_or_else(|| "unbounded".into())
        );
        let _ = writeln!(out, "  reachable:    {}", r.reachable);
        if let Some(err) = r.reachability_error.as_ref() {
            let _ = writeln!(out, "  error:        {err}");
        }
        if let Some(n) = r.xlen {
            let _ = writeln!(out, "  xlen:         {n}");
        }
        if let Some(id) = r.oldest_ts.as_ref() {
            let _ = writeln!(out, "  oldest_id:    {id}");
        }
        if let Some(id) = r.newest_ts.as_ref() {
            let _ = writeln!(out, "  newest_id:    {id}");
        }
        if let Some(ms) = r.last_probe_ms {
            let _ = writeln!(out, "  probe_ms:     {ms}");
        }
        if r.near_max_len {
            let _ = writeln!(out, "  ⚠ stream is ≥95% of max_len — older records will be trimmed soon");
        }
    } else {
        let _ = writeln!(out);
        let _ = writeln!(out, "Redis: not configured (set DARKMUX_REDIS_URL to enable)");
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "Disk");
    let _ = writeln!(out, "  flows_dir:    {}", status.disk.flows_dir);
    let _ = writeln!(out, "  exists:       {}", status.disk.exists);
    let _ = writeln!(out, "  day_files:    {}", status.disk.day_files);
    let _ = writeln!(out, "  total_bytes:  {}", status.disk.total_bytes);

    let _ = writeln!(out);
    let _ = writeln!(out, "Schema");
    let _ = writeln!(out, "  writer:       {}", status.schema.writer_version);
    if status.schema.observed_versions.is_empty() {
        let _ = writeln!(out, "  observed:     (none)");
    } else {
        let _ = writeln!(out, "  observed:     {}", status.schema.observed_versions.join(", "));
    }
    let _ = writeln!(out, "  skew:         {}", status.schema.skew_detected);
    if let Some(reason) = status.schema.skew_reason.as_ref() {
        let _ = writeln!(out, "  reason:       {reason}");
    }

    if !status.warn_reasons.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Warnings:");
        for r in &status.warn_reasons {
            let _ = writeln!(out, "  - {r}");
        }
    }
    if !status.fail_reasons.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Failures:");
        for r in &status.fail_reasons {
            let _ = writeln!(out, "  - {r}");
        }
    }

    out
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
            machine_id: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
                orchestrator: None,
                prev_hash: None,
                hash: None,
                payload: None,
                machine_tier: None,
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
    fn flow_schema_version_is_1_5_0() {
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
        assert_eq!(FLOW_SCHEMA_VERSION, "1.8.0");
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
                orchestrator: None,
                prev_hash: None,
                hash: None,
                payload: None,
                machine_tier: None,
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
            orchestrator: Some("claude-opus-4-7".to_string()),
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
            orchestrator: None,
            prev_hash: Some("seed".to_string()),
            hash: None,
            payload: None,
            machine_tier: None,
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
            orchestrator: None,
            prev_hash: Some("seed".to_string()),
            hash: None,
            payload: None,
            machine_tier: None,
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
        let mut diff_machine_tier = base.clone();
        diff_machine_tier.machine_tier = Some("inference".to_string());
        assert_ne!(
            audit_hash_of(&diff_machine_tier).unwrap(),
            h1,
            "machine_tier must contribute to audit hash"
        );

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
    /// fields (machine_tier / work_id / attempt) must still validate
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
                orchestrator: None,
                prev_hash: None,
                hash: None,
                payload: None,
                machine_tier: None,
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
            !raw.contains("machine_tier"),
            "None-valued machine_tier must be omitted from serialized form (skip_serializing_if). \
             Otherwise pre-1.8 audit chains break under 1.8 reader. Raw:\n{raw}"
        );
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
                orchestrator: None,
                prev_hash: None, // sink stamps this
                hash: None,      // sink stamps this
                payload: None,
                machine_tier: None,
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
                orchestrator: None,
                prev_hash: None,
                hash: None,
                payload: None,
                machine_tier: None,
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
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
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            machine_tier: None,
            work_id: None,
            attempt: None,
        };

        let start = std::time::Instant::now();
        let result = sink.write(&rec);
        let elapsed = start.elapsed();

        // Write MUST fail (no Redis handshake possible); the value
        // we're asserting is the bounded wall-clock.
        assert!(
            result.is_err(),
            "expected failure writing to unresponsive listener; got Ok"
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
