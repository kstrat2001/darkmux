//! Flow record schema + time helpers + provenance resolution.
//!
//! The `FlowRecord` shape, its enum fields (`Level`, `Category`, `Tier`,
//! `Stage`), the per-day file/timestamp helpers, and the env-driven
//! provenance resolvers (`resolve_machine_id` / `resolve_orchestrator`).
//! Split out of the crate's sink/record core (#508).

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub const FLOW_SCHEMA_VERSION: &str = "1.11.0";
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
//   1.9.0 — REMOVED `machine_tier` (the {inference/hub/client} machine-capacity
//           label). It conflated the orchestration `tier` enum with a hardware
//           label that no routing consumed; the capacity concept moves to
//           capability-based model selection driven by the lab-vetted
//           recommendation registry (#321/#322). New records omit the field.
//           Casual LocalFileSink readers are unaffected (unknown keys ignored).
//           Pre-1.9.0 AuditFileSink hash-chains cannot be re-verified after this
//           canonical-form change — rotate to a fresh chain (no-compat-baggage;
//           small known audience). `work_id`/`attempt` are unchanged.
//   (code-internal, no FLOW_SCHEMA_VERSION bump) — removed the orphaned
//           `resolve_machine_tier()` resolver. The `machine_tier` FlowRecord
//           field was already removed in 1.9.0; the resolver lingered with no
//           consumers after the fleet single-stream collapse retired tier
//           routing (#590). No on-the-wire shape change. (#590)
//   1.10.0 — added the `Category::Telemetry` variant (#557 slice 1; the
//           observability-unification keystone). Telemetry folds into the one
//           flow stream as a first-class event family (sources: lms / process /
//           detector / runtime / context / compaction), retiring the separate
//           instruments.jsonl sidecar. Minor + additive: older readers ignore
//           the unknown category; new records only, so prior AuditFileSink
//           chains survive without rotation (unlike the 1.9.0 field removal).
//   1.11.0 — added optional `machine_uid` (#640): the stable hardware
//           identity (IOPlatformUUID), auto-populated at write time. The
//           canonical machine identity, distinct from the mutable `machine_id`
//           label. Older records lack it; the viewer treats absence as
//           *unknown identity* (NOT a fallback to the name). Minor + additive
//           — new records only, prior AuditFileSink chains survive.

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
    /// Telemetry as a first-class flow-event family (#557): per-dispatch
    /// instrument samples — context-fill, detector firings, compaction, lms
    /// load/unload, container CPU — emitted into the one stream, always-on.
    /// Replaces the retired instruments.jsonl sidecar.
    Telemetry,
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
    /// Stable hardware identity of the machine that emitted this record
    /// (`IOPlatformUUID`, #640) — the canonical machine identity, distinct
    /// from the mutable `machine_id` label above. Auto-populated at write time
    /// from `darkmux_hardware::machine_uid()`. `None` off macOS, or on records
    /// written before 1.11.0; the viewer treats absence as *unknown identity*
    /// and groups such records under one "unknown" machine — never falling
    /// back to the (unprovable) name. Schema 1.11 addition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_uid: Option<String>,
    /// Frontier orchestrator driving this record's session — e.g.,
    /// `"claude-code"`, `"antigravity"`, `"cursor"`. Auto-populated from
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
    /// Work-queue claim id when this record was produced by a job that
    /// flowed through the global `darkmux:work` stream. Absent on direct
    /// local dispatches (the operator ran `darkmux crew dispatch <role>`
    /// with no `--machine`). Populated by the dispatch path when it claims
    /// work from the queue. Schema 1.8 addition (#246).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
    /// Retry counter for queued work — 1 on first attempt, 2+ on retries
    /// after lease expiry. Surfaces in `darkmux doctor` as a "recent
    /// retries" rollup. Absent on direct local dispatches (no retry
    /// semantics outside the queue). Schema 1.8 addition (#246).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

/// Resolve the flows directory. Precedence (#661 Slice 3):
/// `env(DARKMUX_FLOWS_DIR) > config.dirs.flows > ~/.darkmux/flows`, with a
/// `/tmp/darkmux/flows` HOME-less (CI / sandbox) fallback. Delegates to the
/// single resolver in `darkmux_types::config_access` so the precedence — now
/// including the config tier — lives in exactly one place.
pub fn flows_dir() -> PathBuf {
    darkmux_types::config_access::flows_dir()
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

pub(crate) fn current_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Convert unix epoch seconds to (year, month, day) in UTC.
/// Civil calendar algorithm from Howard Hinnant (public-domain).
pub(crate) fn epoch_to_yyyymmdd(epochs: i64) -> (i32, u8, u8) {
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
pub(crate) fn epoch_to_hhmmss(epochs: i64) -> (u8, u8, u8) {
    let secs_of_day = epochs.rem_euclid(86_400);
    let h = (secs_of_day / 3600) as u8;
    let mi = ((secs_of_day % 3600) / 60) as u8;
    let s = (secs_of_day % 60) as u8;
    (h, mi, s)
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
    // env(DARKMUX_MACHINE_ID) > config.machine_id (#661 Slice 4). config_access
    // reads the env LIVE per-call, so a `set_var` in tests / operator shells
    // still takes effect without a process restart — the property this hot path
    // (and the serial tests) rely on. The hostname fallback below is unchanged.
    if let Some(id) = darkmux_types::config_access::machine_id() {
        return Some(id);
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
    // env(DARKMUX_ORCHESTRATOR) > config.orchestrator (#661 Slice 4), read live.
    darkmux_types::config_access::orchestrator()
}
