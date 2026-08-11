//! Flow record schema + time helpers + provenance resolution.
//!
//! The `FlowRecord` shape, its enum fields (`Level`, `Category`, `Tier`,
//! `Stage`), the per-day file/timestamp helpers, and the env-driven
//! machine-provenance resolver (`resolve_machine_id`).
//! Split out of the crate's sink/record core (#508).

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub const FLOW_SCHEMA_VERSION: &str = "1.19.0";
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
//   1.12.0 — added the `telemetry.tokens` action + `source=tokens`
//           (#782 token-emission spine): a per-dispatch telemetry record
//           carrying `prompt_tokens` / `completion_tokens` / `total_tokens`
//           in `payload`, read from the runtime's metrics.json at exit. The
//           data the live "tokens off-meter" savings view (#783) aggregates
//           over a window. The same totals also land on the existing
//           `dispatch.complete` Work record's payload. Tokens-only — NO
//           currency by design. Minor + additive: a new action value + new
//           telemetry source, no struct/field change; older readers ignore
//           the unknown action. New records only, prior AuditFileSink chains
//           survive without rotation.
//   1.13.0 — `telemetry.tokens` is now emitted PER TURN by the dispatch
//           tailer (#795) with a new optional `turn_seq` payload field; the
//           per-dispatch at-complete aggregate (1.12.0 / #782) is retired so
//           records sum to the dispatch total without double-counting.
//           Minor + additive: same action + source, new payload field only;
//           consumers that SUM the family are unaffected (old aggregates and
//           new per-turn records sum identically). New records only, chains
//           survive without rotation.
//   1.14.0 — renamed the `Stage::Retrospect` variant → `Stage::Debrief`
//           (serde value `"retrospect"` → `"debrief"`), the NASA-vocabulary
//           rename for the post-mission review stage (#999). The variant was
//           an unemitted placeholder — NO record ever carried the old value —
//           so this changes only the enum's value-set, not any persisted data;
//           AuditFileSink chains survive without rotation (no records change),
//           and the variant stays unemitted until the debrief ceremony (#1000)
//           writes it. Treated as minor since nothing on-the-wire carried the
//           old value; the bump signals the value-set change for that future
//           consumer.
//   1.15.0 — `telemetry.process` (source=process) now samples the HOST system,
//           not the per-dispatch container. Payload gains `mem` + `gpu` (host
//           RAM-used% + GPU-utilization%) alongside `cpu`, and `cpu`'s meaning
//           changes from container-CPU% — which read ~0 because inference runs
//           in LMStudio, off-container (#814/#1064) — to host system-CPU%. Same
//           action + source; additive payload fields (the wire key `cpu` is
//           unchanged, only its source). Older readers ignore `mem`/`gpu`; new
//           records only, so prior AuditFileSink chains survive without rotation.
//           (#1064)
//   1.16.0 — `dispatch.tool` payload gains `args` — the ACTUAL tool arguments
//           (search pattern / file path / shell command), capped at 512 chars
//           in the runtime, so the operator can recall WHAT each tool call did,
//           not just its size. Results stay size-only (large + re-derivable).
//           Additive payload field; older readers ignore `args`; new records
//           only, so prior AuditFileSink chains survive without rotation.
//   1.17.0 — new action values for the review-funnel driver's run
//           observability (#1247 Part 1, `darkmux_lab::lab::funnel`):
//           `funnel.task` (one run's started/finished bookends), `funnel.step`
//           (a step transition — bundle/probe/probe:<seat>/dedup/judge-pass1/
//           judge-pass2 — payload shape `{step_id, kind, items_in, items_out,
//           status, wall_ms}` per #1230's named substrate), and `funnel.ruling`
//           (the per-judge-ruling live ticker). `status` on both task and step
//           is `started` | `finished` | `error` — `error` is the abort-path
//           terminal value the funnel's bookend guard emits on early return /
//           panic (same guarantee #717's DispatchBookendGuard gives
//           `dispatch.start`), so no consumer ever sees an orphaned `started`.
//           No struct/field change — same `payload` blob every other richer
//           action already uses. Additive: older readers ignore the unknown
//           actions. Emitted through TWO sinks depending on caller
//           (lab-vs-fleet scope boundary): `darkmux mission launch review` writes to
//           the real flow stream via this crate; `darkmux lab review-bench
//           --funnel` writes to a per-run-local `funnel-events.jsonl` file
//           instead, never this stream — so existing AuditFileSink chains are
//           unaffected either way.
//   (code-internal, no FLOW_SCHEMA_VERSION bump) — a `dispatch complete`
//           record's payload now carries `endpoint` alongside `remote_tokens`
//           whenever the dispatch involved a remote-endpoint seat (#1230
//           Packet 0, the new `crate::bookend::stamp_remote_classification`
//           helper). `endpoint` already exists on `dispatch.start`/
//           `dispatch.complete` for the container/direct paths since #1187;
//           this only fixes `src/pr_review.rs`'s funnel→dispatch bookend
//           bridge (`with_dispatch_bookends`), which stamped `remote_tokens`
//           alone — the viewer's `tokensOffMeter()` reads `payload.endpoint`
//           exclusively to classify a session as cloud vs. local, so the
//           bridge's records previously counted 100% of a remote-seat
//           funnel run as local savings. Purely additive payload key on an
//           existing action; older readers ignore it, and a fully-local
//           dispatch's payload is byte-identical to before (both fields
//           stay absent).
//   (code-internal, no FLOW_SCHEMA_VERSION bump) — #1349: the review
//           pipeline's module (`darkmux_lab::lab::funnel` -> `::lab::review`)
//           and its `funnel.task`/`funnel.step`/`funnel.ruling` action
//           vocabulary from 1.17.0 above are renamed to the `review.`
//           `{task,step,ruling}` family — "funnel" described a separate,
//           bespoke execution mechanism that #1348 retired (the pipeline is
//           just a mission now, like any other); the name outlived the
//           thing it named. Same payload shapes, same semantics, action
//           STRING only. #1349 also retired the redundant task-level
//           bookend `run_review_graph` (nee `run_funnel_graph`) used to open
//           from INSIDE the pipeline's own top-level call — every
//           production caller already wraps that call in
//           `src/pr_review.rs`'s `with_dispatch_bookends`, which opens the
//           canonical `dispatch start`/`dispatch complete`/`dispatch error`
//           bookend around it (#1230 Packet 0); the inner per-run task
//           bookend now fires ONLY from the still-used sequential
//           `--charges-file` driver (`run_judge_only` — its sibling
//           `run_review` was deleted as dead code in #1357), never from the
//           Task/Step graph driver. Older readers that don't recognize the
//           renamed actions degrade the same way 1.17.0 documented:
//           additive, unknown-action-tolerant.
//   (code-internal, no FLOW_SCHEMA_VERSION bump) — #1434: the review
//           pipeline's bespoke per-run task/step/ruling action vocabulary
//           (the two prior entries above) is RETIRED entirely. The sequential
//           `--charges-file` driver folded onto the SAME generic `step result`
//           companion vocabulary the Task/Step graph driver already emitted,
//           so exactly one review record vocabulary now exists. These records
//           are per-run-local/ephemeral (no versioned consumer), so no bump
//           and no migration — the vocabulary simply stops being written.
//   1.18.0 — live seat-card metrics for AGENTIC seats (#1483 emit half; the
//           render half shipped in #1485/#1488). The live trajectory tailer's
//           per-event records gain optional `payload.step_id` — the
//           mission-graph STEP this dispatch runs as — so the viewer can
//           attribute the live turn/tool/token climb to the seat card even
//           when `session_id` isn't the `step-<id>` default (the coder-phase
//           `mission.coder` seat dispatches under a shared `mission-run-<…>`
//           session, so its live records were previously unattributable and
//           the agentic seat never ticked). Alongside it, `dispatch.turn`
//           gains `turns_so_far` and `dispatch.tool` gains `tool_calls_so_far`
//           — the AUTHORITATIVE monotonic running counts the viewer's seat
//           meter ticks off (so a page opened mid-dispatch reads the true
//           count, not an under-count from the tail-from-now stream). Same
//           actions, same `payload` blob — additive optional fields only;
//           older readers ignore them; new records only, so prior AuditFileSink
//           chains survive without rotation.
//   1.19.0 — REMOVED `orchestrator` (#1758). It was stamped from
//           `config.json` — MACHINE scope — to describe which frontier
//           orchestrator drove the work — INVOCATION scope — so every
//           record on a machine carried the same value regardless of
//           whether it came from an orchestrator session, a hand-typed
//           CLI command, a cron job, or the CI runner. Grepped every
//           consumer: nothing ever READ it (no viewer lens, no CLI verb,
//           no aggregation; `darkmux doctor` only checked "is it
//           declared"). A field that lies is worse than an absent one.
//           Same shape of removal as 1.9.0's `machine_tier`. Casual
//           LocalFileSink readers are unaffected (unknown keys ignored
//           on read; `#[serde(flatten)] extras` on `DarkmuxConfig`
//           absorbs an old `config.json`'s `"orchestrator"` key the same
//           way). Pre-1.19.0 AuditFileSink hash-chains cannot be
//           re-verified after this canonical-form change — rotate to a
//           fresh chain (no-compat-baggage; small known audience). If
//           the want comes back, it comes back differently: a truthful
//           version would stamp per-invocation from process environment
//           (`CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT`, …), not from
//           machine-scoped config — build that when a real consumer
//           needs it.

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
    /// (#1611) Lenient-on-read catch-all. Without it, a variant this binary does
    /// not know fails serde for the WHOLE record — `category` is a required,
    /// non-`Option` field, so one unrecognized string makes the entire
    /// `FlowRecord` unparseable rather than partially readable.
    ///
    /// The schema's own version history promised this already ("older readers
    /// ignore the unknown category", FLOW_SCHEMA 1.10.0) — true for the additive
    /// `Option` fields and the free-form `action` string, and never true for
    /// these enums until now. Contract 5: consumers are lenient-on-read, loud in
    /// doctor. The typed production readers this makes whole are
    /// `integrity_check_file` and `darkmux-crew`'s role index.
    ///
    /// Nothing ever WRITES `Unknown` — no code constructs it — so no record is
    /// emitted carrying one. But reading is lossy where writing is not: a record
    /// parsed to `Unknown` **re-serializes as `"unknown"`**, not as whatever the
    /// writer wrote.
    ///
    /// That loss is why leniency ALONE does not make the audit chain honest, and
    /// the first version of this change wrongly claimed it did.
    /// `audit_hash_of` recomputes the BLAKE3 from the PARSED struct, so a
    /// newer peer's record hashes differently and the checker called it
    /// `hash mismatch — record content has been edited`. That is a false tamper
    /// alert on the compliance substrate, and a worse-reading one than the
    /// `unparseable JSON` it replaced. The real fix lives in
    /// `integrity_check_file`, which now asks `has_unknown_enum` first and
    /// reports such a record as unverifiable-pending-upgrade rather than
    /// corrupt. Any FUTURE consumer that round-trips a record owes the same
    /// question.
    #[serde(other)]
    #[value(skip)]
    Unknown,
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
    /// See [`Level::Unknown`] — same lenient-on-read contract.
    #[serde(other)]
    #[value(skip)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Operator,
    Frontier,
    Local,
    /// See [`Level::Unknown`] — same lenient-on-read contract.
    #[serde(other)]
    #[value(skip)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    Scope,
    Estimate,
    Dispatch,
    Review,
    Ship,
    /// Post-mission review stage (#999, NASA vocabulary — Mission · Crew ·
    /// Debrief · Lessons). The mission debrief ceremony (#1000) distills the
    /// mission's cautions + corrections into durable lessons; records of that
    /// review carry this stage. Serialized as `"debrief"`. Renamed from the
    /// unemitted `Retrospect` placeholder in FLOW_SCHEMA 1.14.0.
    Debrief,
    /// Tier-decision record (#136): the frontier orchestrator's reasoning
    /// for routing this piece of work to local vs. holding in frontier.
    /// Emitted via `darkmux flow tier-decision`. Category typically
    /// `audit`; the `reasoning` field carries the operator-visible
    /// rationale. Serialized as `"tier-decision"`.
    TierDecision,
    /// See [`Level::Unknown`] — same lenient-on-read contract.
    #[serde(other)]
    #[value(skip)]
    Unknown,
}

/// (#1611) Did this record carry an enum spelling THIS binary does not know?
///
/// The catch-all variants make such a record READABLE, which is contract 5 and
/// the whole point. What they cannot do is make it re-serializable to its
/// original bytes: an unknown `"catastrophe"` reads as `Unknown` and writes
/// back as `"unknown"`. Any consumer that round-trips a record — most
/// consequentially `integrity_check_file`, which recomputes the audit hash from
/// the PARSED struct — must therefore be able to ask whether the value it holds
/// is lossy, or it will report its own lossy re-serialization as evidence the
/// record was edited.
///
/// A predicate rather than a stored flag so it cannot go stale, and so nothing
/// has to be added to the wire format to support it.
pub trait MaybeUnknown {
    /// `true` when this value came from a spelling this binary does not know.
    fn is_unknown(&self) -> bool;
}

impl MaybeUnknown for Level {
    fn is_unknown(&self) -> bool {
        matches!(self, Level::Unknown)
    }
}
impl MaybeUnknown for Category {
    fn is_unknown(&self) -> bool {
        matches!(self, Category::Unknown)
    }
}
impl MaybeUnknown for Tier {
    fn is_unknown(&self) -> bool {
        matches!(self, Tier::Unknown)
    }
}
impl MaybeUnknown for Stage {
    fn is_unknown(&self) -> bool {
        matches!(self, Stage::Unknown)
    }
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
    /// Sprint→Phase rename read-compat: historical flow records on disk
    /// (append-only JSONL, never rewritten) carry this under the pre-
    /// rename wire key `sprint_id`. `alias` lets readers accept either
    /// key so historical records don't silently lose the field; every
    /// newly-written record emits the canonical `phase_id` key.
    #[serde(skip_serializing_if = "Option::is_none", alias = "sprint_id")]
    pub phase_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// LMStudio model id that handled this work, when known. Set on
    /// dispatch records (`tier=local, stage=dispatch`) so the viewer
    /// can render which model ran the work without cross-referencing
    /// the model-status pill's timestamp. Resolved at dispatch entry
    /// from the active profile (`crew::select::select_model`). None for
    /// non-dispatch records (lifecycle transitions, phase review
    /// verdicts) and for dispatches where the model can't be resolved.
    /// Schema 1.2 addition (#106).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Operator-facing reasoning for this record. Used primarily by
    /// tier-decision records (#136) where the frontier orchestrator
    /// explains WHY work was routed to local vs. held in frontier. The
    /// audit substrate's "why" layer. Schema 1.3 addition.
    ///
    /// Non-tier-decision records typically leave this `None`. When set
    /// on any record, it's free-form prose intended for human review
    /// (debrief, compliance audit, post-mortem).
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
    /// Together with `prev_hash` forms a hash chain. The
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
    /// local dispatches (the operator ran `darkmux dispatch <role>`
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

impl FlowRecord {
    /// (#1611) `true` when any of this record's enum fields fell back to its
    /// catch-all — i.e. the record was written by a binary that knows a
    /// spelling this one does not.
    ///
    /// Such a record is fully READABLE (that is the point of the catch-alls)
    /// but NOT byte-reproducible: re-serializing it emits `"unknown"` in place
    /// of whatever the writer wrote. Consumers that compare a re-serialization
    /// against something the writer produced — the audit chain's content hash
    /// is the one that matters — must consult this first, or they will report
    /// their own lossy round-trip as evidence of tampering.
    pub fn has_unknown_enum(&self) -> bool {
        self.level.is_unknown()
            || self.category.is_unknown()
            || self.tier.is_unknown()
            || self.stage.is_unknown()
    }
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
/// dispatch / phase timing surfaces; finer precision is a future bump.
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


#[cfg(test)]
mod forward_compat_tests {
    use super::*;

    /// (#1611) A record from a NEWER peer must still parse. Before the
    /// `#[serde(other)]` catch-alls, one unrecognized enum string failed the
    /// whole `FlowRecord`.
    ///
    /// Parsing is the FLOOR, not the fix. The same record still does not
    /// re-serialize to its original bytes — see `unknown_variants_are_lossy_on_
    /// write_which_is_why_the_checker_asks` below, and the integrity checker's
    /// `has_unknown_enum` branch that consumes this.
    #[test]
    fn a_record_from_a_newer_peer_parses_instead_of_failing_the_whole_record() {
        let wire = r#"{
            "ts": "2026-08-03T12:00:00Z",
            "level": "catastrophe",
            "category": "quantum",
            "tier": "orbital",
            "stage": "teleport",
            "action": "dispatch start",
            "handle": "seat-1",
            "session_id": "task-t1",
            "model": "gpt-4o"
        }"#;
        let rec: FlowRecord =
            serde_json::from_str(wire).expect("an unknown enum variant must not fail the record");

        // The unknown values degrade to Unknown; everything else survives, which
        // is the whole point — a reader keeps the fields it understands.
        assert!(matches!(rec.level, Level::Unknown));
        assert!(matches!(rec.category, Category::Unknown));
        assert!(matches!(rec.tier, Tier::Unknown));
        assert!(matches!(rec.stage, Stage::Unknown));
        assert_eq!(rec.action, "dispatch start");
        assert_eq!(rec.session_id.as_deref(), Some("task-t1"));
        assert_eq!(rec.model.as_deref(), Some("gpt-4o"));
    }

    /// Known variants are untouched — the catch-all must not swallow a
    /// variant this binary DOES understand.
    #[test]
    fn known_variants_still_round_trip_exactly() {
        let wire = r#"{
            "ts": "2026-08-03T12:00:00Z",
            "level": "info",
            "category": "telemetry",
            "tier": "local",
            "stage": "dispatch",
            "action": "telemetry.tokens",
            "handle": "seat-1"
        }"#;
        let rec: FlowRecord = serde_json::from_str(wire).unwrap();
        assert!(matches!(rec.level, Level::Info));
        assert!(matches!(rec.category, Category::Telemetry));
        assert!(matches!(rec.tier, Tier::Local));
        assert!(matches!(rec.stage, Stage::Dispatch));
        // And a re-serialize keeps the wire spelling — true for variants this
        // binary KNOWS. It is emphatically not true for unknown ones; that
        // asymmetry is the subject of the next test.
        let out = serde_json::to_value(&rec).unwrap();
        assert_eq!(out["category"], "telemetry");
        assert_eq!(out["stage"], "dispatch");
    }

    /// The correction. Leniency makes a newer record READABLE; it does not make
    /// it REPRODUCIBLE — and the audit chain hashes a re-serialization, so the
    /// difference is the difference between "verified" and "tampered".
    ///
    /// This is the test that would have caught the original overclaim: the
    /// first version of #1611 asserted the catch-alls fixed the false tamper
    /// alert, and they did not. They changed its wording from `unparseable
    /// JSON` to `hash mismatch — record content has been edited`, which reads
    /// MORE like tampering.
    #[test]
    fn unknown_variants_are_lossy_on_write_which_is_why_the_checker_asks() {
        let wire = r#"{
            "ts": "2026-08-03T12:00:00Z",
            "level": "catastrophe",
            "category": "audit",
            "tier": "local",
            "stage": "dispatch",
            "action": "dispatch start",
            "handle": "seat-1"
        }"#;
        let rec: FlowRecord = serde_json::from_str(wire).unwrap();
        let out = serde_json::to_value(&rec).unwrap();

        assert_eq!(
            out["level"], "unknown",
            "the writer's spelling is GONE on re-serialize — this is the lossiness \
             that makes a recomputed content hash differ for an untouched record"
        );
        assert_ne!(out["level"], "catastrophe");

        // ...which is exactly the question any round-tripping consumer must
        // ask, and the integrity checker now does.
        assert!(rec.has_unknown_enum());

        // A record this binary fully understands is NOT flagged, so the
        // checker keeps verifying everything it legitimately can.
        let known: FlowRecord = serde_json::from_str(
            r#"{"ts":"2026-08-03T12:00:00Z","level":"info","category":"audit",
                "tier":"local","stage":"dispatch","action":"a","handle":"h"}"#,
        )
        .unwrap();
        assert!(!known.has_unknown_enum());
    }
}
