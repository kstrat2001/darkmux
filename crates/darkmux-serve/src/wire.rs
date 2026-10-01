//! The daemon's response bodies: one Rust type per JSON route.
//!
//! From this release the daemon's routes and response shapes are semver
//! contracts. Every JSON body a handler returns is one of the types below (or a
//! type that lives beside the thing it describes and is named in
//! `routes::TABLE`), and the TypeScript the viewer imports is generated from
//! them (`bun run types:regen`). A handler builds no `json!` object by hand: a
//! field added here changes the generated twin, and a route's response type is
//! pinned by the route-table golden.
//!
//! Fields the producer could not read are `null` on the wire, never a zero or an
//! empty string; `Option` fields marked `skip_serializing_if` are absent
//! instead, and the generated field is optional.

use darkmux_crew::types::{Mission, Phase};
use darkmux_flow::presence::PresenceBeat;
use darkmux_flow::session_presence::SessionBeat;
use darkmux_profiles::model_ledger::ModelLedger;
use darkmux_types::config::BusyPolicy;
use darkmux_types::LoadedModel;
use serde::{Deserialize, Serialize};

use crate::runs::{Run, RunsPolicy};
use crate::source_state::SourceState;

// ─── /health ───────────────────────────────────────────────────────────────

/// `GET /health`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct HealthResponse {
    pub darkmux_version: String,
    /// The package version plus the git short SHA.
    pub build: String,
    /// When the binary this daemon loaded was last written, epoch seconds. A bare
    /// integer, never the exe path: `/health` is auth-exempt.
    #[cfg_attr(test, ts(type = "number | null"))]
    pub binary_mtime: Option<u64>,
    pub flow_schema_version: String,
    /// What the fleet listener is doing (`off` when `fleet.listener.enabled` is
    /// false): the detail for a caller on this machine, a coarse state for
    /// anyone else.
    pub fleet_listener: Option<String>,
    /// The busy policy and hosted-job bound the running listener uses; this
    /// machine only.
    pub fleet_busy: Option<FleetBusy>,
    /// Whether this daemon resolved a fleet token (the serve token) in its own
    /// environment; never the value. This machine only.
    pub fleet_token_set: Option<bool>,
    /// Whether this daemon can publish to the fleet hub's flow stream, and
    /// since when it could not; `null` when no hub is configured. This machine
    /// only.
    pub hub_link: Option<darkmux_flow::HubLink>,
    /// The open-file soft limit this daemon runs with; this machine only.
    #[cfg_attr(test, ts(type = "number | null"))]
    pub open_file_limit: Option<u64>,
    /// The lifecycle policy every run is judged by.
    pub lifecycle_policy: RunsPolicy,
    pub live: HealthLive,
}

/// The fleet listener's busy policy, as running.
#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FleetBusy {
    pub policy: BusyPolicy,
    pub hosted_cap: u32,
}

/// The live channel as this daemon runs it: the cadence knob and the ingest's
/// own counters, so its cost and traffic are readable without a debugger.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct HealthLive {
    #[cfg_attr(test, ts(type = "number"))]
    pub sample_ms: u64,
    /// Which socket this daemon bound; `null` when none.
    pub ingest: Option<LiveIngestHealth>,
    #[cfg_attr(test, ts(type = "number"))]
    pub received: u64,
    #[cfg_attr(test, ts(type = "number"))]
    pub rejected: u64,
    #[cfg_attr(test, ts(type = "number"))]
    pub handle_us: u64,
    #[cfg_attr(test, ts(type = "number"))]
    pub viewers: usize,
}

/// The live-sample ingest socket, by fingerprint and port.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct LiveIngestHealth {
    pub socket_id: String,
    pub socket_port: u16,
    pub bound: bool,
}

// ─── /machine/* ────────────────────────────────────────────────────────────

/// `GET /machine/status`: the resident models.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MachineStatusResponse {
    pub models: Vec<LoadedModel>,
    /// The `lms` binary could not be invoked.
    pub lms_unreachable: bool,
    #[cfg_attr(test, ts(type = "number"))]
    pub generated_at_ms: u64,
}

/// The configured machine utility model and whether it is resident.
#[derive(Debug, Clone, Serialize, serde::Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct UtilityModel {
    pub id: String,
    pub loaded: bool,
    /// The binding's declared window (`internal.utility.n_ctx`); `null` for a
    /// bare binding.
    pub n_ctx: Option<u32>,
}

/// `GET /machine/specs`.
#[derive(Debug, Clone, Serialize, serde::Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MachineSpecsResponse {
    pub darkmux_version: String,
    pub flow_schema_version: String,
    /// The operator-chosen machine name; `null` when none resolves.
    pub machine_id: Option<String>,
    /// This daemon's stable hardware identity, the same probe that keys every
    /// presence beat and stamps every flow record. `null` off macOS or when the
    /// probe fails; a consumer keeps its name-based path as the fallback and
    /// never reads absence as "not this machine".
    pub machine_uid: Option<String>,
    pub os: String,
    #[cfg_attr(test, ts(type = "number | null"))]
    pub ram_total_bytes: Option<u64>,
    #[cfg_attr(test, ts(type = "number | null"))]
    pub ram_free_for_ai_bytes: Option<u64>,
    pub cpu_brand: Option<String>,
    pub loaded_models: Vec<LoadedModel>,
    pub lms_unreachable: bool,
    pub utility_model: Option<UtilityModel>,
    /// The Redis URL with its password redacted, when Redis is configured.
    pub redis_url_redacted: Option<String>,
    #[cfg_attr(test, ts(type = "number"))]
    pub generated_at_ms: u64,
}

/// `GET /machine/resources`: the memory ledger, the recorded cache cadence, and
/// the host sampler's reading when one has landed.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MachineResourcesResponse {
    #[serde(flatten)]
    #[cfg_attr(test, ts(flatten))]
    pub ledger: ModelLedger,
    /// The recorded cadence knob (the observer's own cost is never adaptive).
    #[cfg_attr(test, ts(type = "number"))]
    pub cache_ttl_ms: u64,
    /// Absent until the sampler has produced a sample (it is off with
    /// `runtime.host_sampler_interval_ms: 0`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub load: Option<MachineLoad>,
}

/// The daemon-side continuous host sampler's reading.
pub use darkmux_flow::payload::{LoadWindow, MachineLoad};

// ─── coverage ──────────────────────────────────────────────────────────────

/// The sources whose completeness is tracked. An absent key means "not
/// tracked", never "fine".
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CoverageSources {
    pub fleet: SourceState,
}

/// The `meta` every coverage-bearing response carries.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CoverageMeta {
    pub sources: CoverageSources,
    /// Derived "is this the whole truth?", so a renderer decides whether to warn
    /// without re-deriving each state's meaning.
    pub complete: bool,
}

/// [`CoverageMeta`] plus what a catalog scan cost.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct RecordsMeta {
    #[serde(flatten)]
    #[cfg_attr(test, ts(flatten))]
    pub coverage: CoverageMeta,
    #[cfg_attr(test, ts(type = "number"))]
    pub scan_ms: u64,
    #[cfg_attr(test, ts(type = "number"))]
    pub days_scanned: usize,
    #[cfg_attr(test, ts(type = "number"))]
    pub records_scanned: usize,
}

// ─── /runs, /missions, /phases ─────────────────────────────────────────────

/// `GET /runs`: the flat, kind-tagged run view-model.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct RunsResponse {
    pub runs: Vec<Run>,
    #[cfg_attr(test, ts(type = "number"))]
    pub generated_at_ms: u64,
    pub meta: CoverageMeta,
    pub policy: RunsPolicy,
}

/// `GET /missions`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MissionsResponse {
    pub missions: Vec<Mission>,
    #[cfg_attr(test, ts(type = "number"))]
    pub generated_at_ms: u64,
}

/// `GET /phases`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct PhasesResponse {
    pub phases: Vec<Phase>,
    #[cfg_attr(test, ts(type = "number"))]
    pub generated_at_ms: u64,
}

// ─── the flow catalog ──────────────────────────────────────────────────────

/// One day file on disk.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FlowDay {
    pub date: String,
    #[cfg_attr(test, ts(type = "number"))]
    pub records: u64,
    pub missions: Vec<String>,
    #[cfg_attr(test, ts(type = "number"))]
    pub dispatches: usize,
}

/// `GET /flow-days`: newest first.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FlowDaysResponse {
    pub days: Vec<FlowDay>,
    #[cfg_attr(test, ts(type = "number"))]
    pub generated_at_ms: u64,
}

/// A cross-day rollup for one `mission_id`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FlowMissionSummary {
    pub mission_id: String,
    #[cfg_attr(test, ts(type = "number"))]
    pub records: u64,
    #[cfg_attr(test, ts(type = "number"))]
    pub dispatches: usize,
    pub machines: Vec<String>,
    pub first_ts: String,
    pub last_ts: String,
    pub first_date: String,
    pub last_date: String,
}

/// `GET /flow-missions`: newest activity first.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FlowMissionsResponse {
    pub missions: Vec<FlowMissionSummary>,
    #[cfg_attr(test, ts(type = "number"))]
    pub generated_at_ms: u64,
    pub meta: CoverageMeta,
}

/// The TypeScript shape of one raw archive line. It is `FlowRecord` with
/// `source` and `stage` opened to any string.
///
/// The archive is append-only and the legacy reader keeps 3.x values verbatim
/// by design (`source: "mission"` and `"review"` are what the run-grain
/// derivation keys on; a free-text `--source` or a retired `stage` such as
/// `"estimate"` also survives), so those two fields can hold a value the
/// closed `FlowSource` and `Stage` unions do not list. The viewer's ingest
/// module normalizes them to open tags. Only a TypeScript name: the server
/// sends the lines as JSON values and never builds one of these.
#[cfg(test)]
#[derive(ts_rs::TS)]
#[ts(
    export,
    export_to = "../../../ui/src/types/generated/",
    type = "Omit<import(\"./FlowRecord\").FlowRecord, \"source\" | \"stage\"> & { source?: import(\"./FlowSource\").FlowSource | string, stage: import(\"./Stage\").Stage | string }"
)]
pub struct ArchiveFlowRecord;

/// `GET /flow-mission/:id` and `GET /flow-dispatch/:id`: the records of one
/// mission or one dispatch, across days and fleet.
///
/// The records are the raw archive lines (an archive line may carry a field this
/// binary does not model), so the field holds JSON values whose documented shape
/// is `ArchiveFlowRecord`, not `FlowRecord`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FlowRecordsResponse {
    #[cfg_attr(test, ts(as = "Vec<ArchiveFlowRecord>"))]
    pub records: Vec<serde_json::Value>,
    #[cfg_attr(test, ts(type = "number"))]
    pub count: usize,
    /// The record cap cut the answer short.
    pub truncated: bool,
    #[cfg_attr(test, ts(type = "number"))]
    pub generated_at_ms: u64,
    pub meta: RecordsMeta,
}

// ─── /fleet/* ──────────────────────────────────────────────────────────────

/// `GET /fleet/machines/live`: the machines beating right now.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FleetMachinesLiveResponse {
    pub machines: Vec<PresenceBeat>,
    pub meta: CoverageMeta,
}

/// `GET /fleet/dispatches/live`: the dispatches with a live heartbeat.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FleetDispatchesLiveResponse {
    pub dispatches: Vec<SessionBeat>,
    pub meta: CoverageMeta,
}

/// One machine in the operator's DECLARED roster (`fleet.json`).
#[derive(Debug, Clone, Serialize, serde::Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct RosterMachineEntry {
    pub id: String,
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub description: Option<String>,
    #[cfg_attr(test, ts(type = "number"))]
    pub added_unix_ms: u64,
    /// The machine's hardware identity: declared, or derived from the flow
    /// history when the entry declares none. Absent means unknown identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub machine_uid: Option<String>,
    /// Added with `machine add --allow-loopback`: the loopback address is
    /// intentional. Omitted when false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub loopback_intended: Option<bool>,
}

/// `GET /fleet/roster`: the declared topology, independent of presence.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FleetRosterResponse {
    pub machines: Vec<RosterMachineEntry>,
    /// Non-null only when the roster file EXISTS but failed to parse; a missing
    /// file is an empty roster with `null`.
    pub error: Option<String>,
}

// ─── /lab/* ────────────────────────────────────────────────────────────────

/// One seat of a lab run's staffing: what the run used, not how it was chosen.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct LabSeat {
    pub name: String,
    pub model: String,
    pub k: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub n_ctx: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub max_tokens: Option<u32>,
}

/// The seats a lab run actually used, as the runs lens diffs them.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct LabStaffing {
    pub probes: Vec<LabSeat>,
    pub judge: Option<LabSeat>,
}

impl From<&darkmux_crew::run_record::SeatStaffingSnapshot> for LabSeat {
    fn from(s: &darkmux_crew::run_record::SeatStaffingSnapshot) -> Self {
        Self { name: s.name.clone(), model: s.model.clone(), k: s.k, n_ctx: s.n_ctx, max_tokens: s.max_tokens }
    }
}

impl From<&darkmux_crew::run_record::StaffingSnapshot> for LabStaffing {
    fn from(s: &darkmux_crew::run_record::StaffingSnapshot) -> Self {
        Self { probes: s.probes.iter().map(LabSeat::from).collect(), judge: s.judge.as_ref().map(LabSeat::from) }
    }
}

/// `GET /lab/runs`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct LabRunsResponse {
    /// `false` (with empty `runs`) means the daemon has no lab dir wired.
    pub configured: bool,
    pub dir: Option<String>,
    pub exists: bool,
    pub runs: Vec<crate::LabRunSummary>,
    /// Present while runs recorded before 4.0 still sit in the old lab dir.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub pending_move: Option<crate::PendingMove>,
}

/// One review envelope's headline, from a run's archived `funnels.json`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct LabReviewSummary {
    pub case_id: String,
    pub crew: String,
    pub mode: String,
    #[cfg_attr(test, ts(type = "number"))]
    pub confirmed: usize,
    #[cfg_attr(test, ts(type = "number"))]
    pub needs_check: usize,
    #[cfg_attr(test, ts(type = "number"))]
    pub archived: usize,
}

/// Marks that the run wrote a readable `scores.json`; it carries no fields
/// (its `role`/`mode`/`profile` were set only by the removed `lab eval`, #3036).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct LabScoresSummary {}

/// `GET /lab/run/detail?dir=`. `reviews` is `[]` (never an error) when the run
/// has no archived review envelopes.
#[derive(Debug, Clone, Serialize, Default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct LabRunDetailResponse {
    pub dir: String,
    pub reviews: Vec<LabReviewSummary>,
    pub scores: Option<LabScoresSummary>,
}

/// `GET /lab/run/events?dir=&offset=`: the poll-based tail. The lines are
/// flow-record-shaped raw archive lines.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct LabRunEventsResponse {
    #[cfg_attr(test, ts(as = "Vec<ArchiveFlowRecord>"))]
    pub lines: Vec<serde_json::Value>,
    #[cfg_attr(test, ts(type = "number"))]
    pub next_offset: u64,
    pub finished: bool,
}

// ─── /panel/:id ────────────────────────────────────────────────────────────

/// `GET /panel/:id`: an allowlisted CLI command's own rendered output. Metadata
/// AROUND the text, never extraction FROM it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct PanelResponse {
    pub panel: String,
    pub argv: Vec<String>,
    /// Every declared opt's RESOLVED value, defaults included. Empty for a panel
    /// with no declared opts.
    pub opts: std::collections::BTreeMap<String, String>,
    #[cfg_attr(test, ts(type = "number"))]
    pub captured_ts_ms: u64,
    /// The observer's own cost.
    #[cfg_attr(test, ts(type = "number"))]
    pub gather_ms: u64,
    pub exit_code: Option<i32>,
    pub ansi_text: String,
    /// Non-empty only when something went to stderr.
    pub stderr_tail: String,
    pub cols: u16,
    #[cfg_attr(test, ts(type = "number"))]
    pub cache_ttl_ms: u64,
    /// How old a cached copy is; `0` for a fresh run.
    #[cfg_attr(test, ts(type = "number"))]
    pub age_ms: u64,
    /// `false` marks a manual-run-only panel (`doctor`).
    pub auto_refresh: bool,
}

impl From<&darkmux_fleet::MachineEntry> for RosterMachineEntry {
    fn from(m: &darkmux_fleet::MachineEntry) -> Self {
        Self {
            id: m.id.clone(),
            address: m.address.clone(),
            description: m.description.clone(),
            added_unix_ms: m.added_unix_ms,
            machine_uid: m.machine_uid.clone(),
            loopback_intended: m.loopback_intended.then_some(true),
        }
    }
}

impl From<&darkmux_lab::lab::review::ReviewEnvelope> for LabReviewSummary {
    fn from(e: &darkmux_lab::lab::review::ReviewEnvelope) -> Self {
        Self {
            case_id: e.case_id.clone(),
            crew: e.crew.clone(),
            mode: e.mode.clone(),
            confirmed: e.confirmed,
            needs_check: e.needs_check,
            archived: e.archived,
        }
    }
}

