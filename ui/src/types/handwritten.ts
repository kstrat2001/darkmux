/**
 * Hand-written wire types for daemon endpoints that build ad-hoc
 * `serde_json::json!({...})` payloads rather than serializing a typed Rust
 * struct — ts-rs has nothing to derive from for these, so per the packet
 * brief they're typed here by hand instead of refactoring the Rust side to
 * appease the type bridge (out of scope for this packet; a genuinely typed
 * response is a `darkmux-serve` follow-up, not a UI-port concern).
 *
 * Each type names its Rust source so a future edit to the handler is the
 * signal to come back and update the type here too — there is no automated
 * drift guard for these (unlike `types/generated/`, which regenerates from
 * the real struct via `bun run types:check`).
 */

import type { Run } from "./generated/Run";

/** `GET /runs` — the wrapper `runs_handler` builds around `Vec<Run>`.
 * Source: `crates/darkmux-serve/src/lib.rs::runs_handler`. */
export interface RunsResponse {
  runs: Run[];
  generated_at_ms: number;
}

/**
 * How completely one underlying source answered for a response (#1729).
 * Source: `crates/darkmux-serve/src/source_state.rs::SourceState`.
 *
 * Four states, and the pair that matters is `off` vs `unavailable`: a
 * standalone machine with no fleet is CORRECT and must never be warned at,
 * while a configured fleet that could not be read is an incomplete answer
 * wearing a complete answer's clothes. Rendering them the same is the whole
 * defect the marker exists to remove.
 */
export type SourceState =
  | { state: "ok" }
  | { state: "stale"; age_ms: number; detail: string }
  | { state: "unavailable"; detail: string }
  | { state: "off" };

/**
 * The `meta` every coverage-bearing endpoint carries (#1729).
 *
 * `sources` holds ONLY sources whose state is genuinely tracked — an absent
 * key means "not tracked", never "fine". `complete` is the derived
 * "is this the whole truth?", so a renderer can decide whether to warn
 * without re-deriving the meaning of each state at every call site.
 */
export interface CoverageMeta {
  sources: { fleet?: SourceState };
  complete: boolean;
}

/** `GET /fleet/machines/live` (#1729) — the beats plus their coverage. */
export interface FleetMachinesLiveResponse {
  machines: PresenceBeat[];
  meta: CoverageMeta;
}

/** `GET /fleet/sessions/live` (#1729) — the beats plus their coverage. */
export interface FleetSessionsLiveResponse {
  sessions: LiveSessionBeat[];
  meta: CoverageMeta;
}

/** One presence beat. (The endpoint wraps these in `FleetMachinesLiveResponse`
 * since #1729; this is the element type.) A REAL typed
 * struct, but one that lives in `darkmux-flow` rather than `darkmux-serve`.
 * Bridging it with ts-rs would mean adding `ts-rs` as a dependency of
 * `darkmux-flow` itself (a lib crate consumed by production code, not just
 * `darkmux-serve`'s test-only surface) — a heavier footprint than this
 * packet's one-Rust-change scope, and it fought the `#[cfg(test)]`-gated
 * pattern the rest of the bridge uses (a struct used from non-test code needs
 * the derive available OUTSIDE `cfg(test)`, which starts pulling ts-rs into
 * the release dependency graph). Hand-written per the rabbit-hole guard;
 * ledgered in the runbook's FOLLOW-UPS section for the machine-lens packet.
 * Source: `crates/darkmux-flow/src/presence.rs::PresenceBeat`. */
export interface PresenceBeat {
  machine_uid: string;
  display_name: string;
  schema_version: string;
  beat_ts_ms: number;
  specs?: string;
  loaded_models?: string[];
}

/** `GET /machine/specs`. Source:
 * `crates/darkmux-serve/src/lib.rs::machine_specs_handler`. */
export interface LoadedModel {
  identifier: string;
  model: string;
  status: string;
  size: string;
  context: number;
}

export interface UtilityModel {
  id: string;
  loaded: boolean;
}

export interface MachineSpecs {
  darkmux_version: string;
  flow_schema_version: string;
  machine_id: string;
  os: string;
  ram_total_bytes: number | null;
  ram_free_for_ai_bytes: number | null;
  cpu_brand: string | null;
  loaded_models: LoadedModel[];
  lms_unreachable: boolean;
  utility_model: UtilityModel | null;
  redis_url_redacted: string | null;
  generated_at_ms: number;
}

/** `GET /machine/resources` — the memory ledger. Source:
 * `crates/darkmux-serve/src/lib.rs::machine_resources_handler`, which
 * serializes `darkmux_profiles::model_ledger::gather`'s output. Only the
 * fields the scaffold's proof region and near-term lens work need are typed
 * here; the ledger carries more (see the corpus fixture at
 * `tests/parity/corpus/machine-resources.json` for the full shape) — widen
 * this interface as a lens packet actually consumes more of it. */
export interface MachineResourcesModel {
  identifier: string;
  model_key: string;
  owner: string;
  loaded_ctx: number;
  weights_bytes: number;
  kv_per_token_bytes: number;
  kv_bytes_at_ctx: number;
  potential_bytes: number;
  /** #1819/#1820 — where `potential_bytes` came from. `"arch"` = MEASURED
   * from the model's own architecture facts, read either from a sibling
   * `config.json` or (#1820) straight out of the GGUF binary header; the
   * two share one value deliberately, because what matters downstream is
   * that the number was measured, not which byte layout carried it.
   * `"estimated"` = the size-based fallback (catalog size + a conservative
   * dense-attention KV constant, #1819's `ArchWithSizeFallback`), used when
   * NEITHER reader could answer — a corrupt or truncated download, an
   * ambiguous multi-file directory, or a weights format neither understands.
   * OMITTED (not `null`) when `potential_bytes` itself is `null` — nothing
   * priced the row at all.
   * Source: `crates/darkmux-profiles/src/model_ledger.rs::ModelRow`. */
  potential_source?: "arch" | "estimated";
  current_bytes: number;
  state: "green" | "yellow" | "red" | string;
  /** #1854 — how much this resident holds ABOVE its priced
   * `potential_bytes`, when that overage is material. OMITTED (not `null`)
   * in the normal case: the resident is at or under its price.
   *
   * The server computes the condition, including the flap floor, so the row
   * hint and the machine caption's count read ONE definition rather than
   * each re-deriving `current > potential` — #1852's lesson about figures
   * whose definition lives in two places.
   *
   * NOT a severity: a row carrying this is not unhealthy, and `state` is
   * deliberately untouched by it. What was falsified is the estimate's
   * ceiling, not the fit.
   * Source: `crates/darkmux-profiles/src/model_ledger.rs::ModelRow`. */
  over_price_bytes?: number;
}

/** `GET /lab/runs`'s per-seat staffing snapshot — only the fields the runs
 * lens's series/knob-diff view actually reads (`labKnobSummary`/
 * `labKnobDiff` in the legacy viewer). `SeatStaffingSnapshot` carries more
 * (`role_id`, `remote`, `endpoint`, `passes`, `selector`, `provenance`) —
 * widen this as a lens actually consumes more of it, per this file's own
 * convention. `model`/`k` are optional (not `| null` — the real payload
 * omits rather than nulls an absent seat field) so a seat missing either one
 * mid-diff (the "+probe"/"-probe" appear/disappear case `labKnobDiff` in
 * `ui/src/lenses/lab/labSeries.ts` exercises) still type-checks; a run's own
 * REAL emitted seats always carry both. Source:
 * `crates/darkmux-lab/src/lab/review.rs::SeatStaffingSnapshot`. */
export interface SeatStaffing {
  name: string;
  model?: string;
  k?: number;
  n_ctx?: number;
  max_tokens?: number;
}

/** Source: `crates/darkmux-lab/src/lab/review.rs::StaffingSnapshot` (the
 * `verify`/`request_changes` fields exist on the real struct but are unread
 * by the runs lens's series view — same widen-when-consumed note as above).
 * `probes` is optional here (the real struct always sends the array, even
 * empty) to match `labKnobSummary`'s own `st.probes || []` null-guard and
 * the lab-series test spec's minimal fixtures (`{ judge: {...} }` alone). */
export interface StaffingSnapshot {
  probes?: SeatStaffing[];
  judge?: SeatStaffing;
}

/** One row of `GET /lab/runs`'s `runs` array — the SAME on-disk scan `/runs`
 * folds lab-bench runs from (see `Run`'s own doc), but carrying the lab-only
 * extras (`staffing` snapshot, bundle/flag counts) the flat `Run` shape
 * doesn't. `crew`/`exec_mode`/`staffing` accept an explicit `null` in
 * addition to `undefined` — `labKnobDiff`'s `(prev.crew || null) !==
 * (curr.crew || null)` comparison treats the two as the same "absent" value
 * (see `labSeries.test.ts`'s differential-tested case for why that matters:
 * comparing the RAW fields instead emits a phantom no-op diff line).
 * `has_funnels`/`has_events` are always present on a real payload (see
 * `tests/parity/corpus/lab-runs.json`) but stay optional here because the
 * runs-lens's own test fixtures predate the field. Source:
 * `crates/darkmux-serve/src/lib.rs::LabRunSummary`. */
export interface LabRun {
  dir: string;
  mtime_ms: number;
  case_ids: string[];
  crew?: string | null;
  exec_mode?: string | null;
  staffing?: StaffingSnapshot | null;
  bundles: number;
  raw_flags: number;
  deduped_flags: number;
  confirmed: number;
  needs_check: number;
  archived: number;
  degenerate: boolean;
  finished: boolean;
  has_funnels?: boolean;
  has_events?: boolean;
}

/** `GET /lab/runs` itself. `configured: false` (with an empty `runs`) means
 * the daemon has no lab-dir source wired — never a 404/500. Source:
 * `crates/darkmux-serve/src/lib.rs::lab_runs_handler`. */
export interface LabRunsResponse {
  configured: boolean;
  dir: string | null;
  exists: boolean | null;
  runs: LabRun[];
}

/** `GET /panel/:id` — an allowlisted CLI command's own rendered output,
 * metadata AROUND the text, never extraction FROM it. Source:
 * `crates/darkmux-serve/src/panel.rs::panel_handler`'s `serde_json::json!`
 * body — see that module's own doc for the full field-by-field rationale
 * (`gather_ms` stamps the observer's own cost; `cache_ttl_ms`/`age_ms` make
 * staleness verifiable; `auto_refresh:false` is `doctor`'s manual-run-only
 * marker, honored client-side AND enforced server-side). */
export interface PanelResponse {
  panel: string;
  argv: string[];
  captured_ts_ms: number;
  gather_ms: number;
  exit_code: number | null;
  ansi_text: string;
  stderr_tail: string;
  cols: number;
  cache_ttl_ms: number;
  age_ms: number;
  auto_refresh: boolean;
}

/** `GET /flow-days` — one row per `YYYY-MM-DD.jsonl` day file on disk,
 * newest-first (the server sorts). Source:
 * `crates/darkmux-serve/src/lib.rs::scan_flow_days`. */
export interface FlowDay {
  date: string;
  records: number;
  missions: string[];
  dispatches: number;
}

/** `GET /flow-days` itself. Source:
 * `crates/darkmux-serve/src/lib.rs::flow_days_handler`. */
export interface FlowDaysResponse {
  days: FlowDay[];
  generated_at_ms: number;
}

/** One row of `GET /flow-missions` — a cross-day rollup per `mission_id`,
 * newest-activity-first (the server sorts by `last_ts`). Source:
 * `crates/darkmux-serve/src/lib.rs::scan_flow_missions`. */
export interface FlowMissionSummary {
  mission_id: string;
  records: number;
  dispatches: number;
  machines: string[];
  first_ts: string;
  last_ts: string;
  first_date: string;
  last_date: string;
}

/** `GET /flow-missions` itself. Source:
 * `crates/darkmux-serve/src/lib.rs::flow_missions_handler`. */
export interface FlowMissionsResponse {
  missions: FlowMissionSummary[];
  generated_at_ms: number;
}

/** `GET /flow-mission/:id` and `GET /flow-session/:id` — the "replay this
 * mission/dispatch" payload, both built by the same handler. `records` is
 * the SAME raw flow-record shape `FlowRecord` already types (widened from
 * the previously-opaque `Record<string, unknown>[]` this packet's own
 * doc-comment named as the widen-when-consumed convention — the session
 * drill-in, `SessionReplay.tsx`, is the "future lens that actually renders
 * the records" that convention anticipated). Source:
 * `crates/darkmux-serve/src/lib.rs::catalog_records_response`. */
export interface FlowRecordsResponse {
  records: FlowRecord[];
  count: number;
  truncated: boolean;
  generated_at_ms: number;
}

/** #1821 — one entry in `MachineResources.messages`, replacing the old
 * uniformly-amber `warnings: string[]`. `info` is a disclosure (the #1819
 * estimate note); `warn` is a real degradation; `error` means the reading
 * itself is untrustworthy (a probe/enumeration failure). Source:
 * `crates/darkmux-profiles/src/model_ledger.rs::{Severity,LedgerMessage}`. */
export interface LedgerMessage {
  severity: "info" | "warn" | "error";
  text: string;
}

export interface MachineResources {
  schema_version: string;
  generated_at_ms: number;
  gather_ms: number;
  limit_bytes: number;
  limit_source: string;
  /** #1821 — a real machine-memory decomposition, all three read from the
   * SAME `vm_stat` call the gather already runs.
   * - `used_bytes` — Activity-Monitor-style: `wired + compressor +
   *   (active + inactive - purgeable)`.
   * - `available_bytes` — the colloquial "how much is left":
   *   `free + inactive + speculative`. This is the headline figure — put
   *   it where the old `pool free` sat.
   * - `free_bytes` — truly-free pages only (`Pages free × page size`). The
   *   figure `available_bytes` used to (wrongly) mean before #1821's
   *   rename. Kept in the payload but deliberately NOT given prime space
   *   next to `available_bytes` in the k/v row — two figures both reading
   *   as "how much is left" was the defect being fixed. */
  pool: { capacity_bytes: number; used_bytes: number; available_bytes: number; free_bytes: number };
  pressure: {
    swap_used_bytes: number;
    compressor_bytes: number;
    /** `sysctl kern.memorystatus_level` — `(capacity - wired - compressor)
     * / capacity`, the kernel's own 0–100 pressure headroom. Named
     * `margin`, NOT `free` (#1821, operator-approved): live, same
     * instant, this read 82% while truly-free pages read 30.8% — neither
     * "free" nor "available" describes it. Renders on the odometer tile
     * as `MARGIN`. Still the sole red trigger (`< 15`). */
    margin_percent: number;
    red: boolean;
  };
  models: MachineResourcesModel[];
  machine: {
    /** #1854 — summed as `max(potential, current)` per resident. A declared
     * maximum sitting below that resident's own measured footprint is a
     * disproved number, not a conservative one; see `over_price_models`. */
    potential_bytes: number;
    unpriced_models: number;
    /** #1819 — residents priced by the size-based fallback rather than
     * measured arch facts. Counted separately from `unpriced_models`: an
     * estimated resident DOES contribute to `potential_bytes` and does NOT
     * block a Green verdict — only `unpriced_models` (genuinely
     * unpriceable, no potential at all) does that. */
    estimated_models: number;
    /** #1854 — residents counted at their MEASURED size because it exceeded
     * the price they declared. It qualifies the verdict rather than changing
     * it: a green with zero here is CEILING-backed (fits even if every
     * resident grows to its declared maximum); a green with a non-zero count
     * is FLOOR-backed (fits at the larger of each price and each observed
     * size, with that many maxima known to be wrong). Same chip, weaker
     * promise — which is why the count renders beside the chip.
     *
     * `?` for a pre-2.1 peer's ledger, matching the server's `serde(default)`. */
    over_price_models?: number;
    current_bytes: number;
    /** #1835 — WHICH disjunct produced an amber `state`; absent for every
     * other verdict. `"overcommitted"` = the projected total exceeds the
     * limit outright (shrink something). `"no_margin"` = it fits, but the
     * headroom left is under the server's margin floor (do not load anything
     * else). The two call for opposite responses, so the lamp row keys on
     * this rather than on a threshold re-derived client-side — the floor
     * lives server-side and only server-side. */
    amber_reason?: "overcommitted" | "no_margin";
    /** #1821 — everything ELSE on the machine, right now:
     * `pool.used_bytes - current_bytes`, floored at 0. */
    other_used_bytes?: number;
    /** #1821 — `other_used_bytes + potential_bytes`. This is what the
     * green/amber cascade actually compares against `limit_bytes` now —
     * "if darkmux's own commitment fully materializes while everything
     * else holds what it holds now, what is the total?" Replaces the old
     * `potential_bytes <= limit_bytes` comparison, which silently assumed
     * darkmux was the machine's only tenant. */
    projected_total_bytes?: number;
    state: "green" | "yellow" | "red" | string;
  };
  attribution: string;
  attribution_note?: string;
  /** #1821 — replaces `warnings: string[]`. See [`LedgerMessage`]. */
  messages: LedgerMessage[];
  cache_ttl_ms: number;
}

/** `GET /fleet/sessions/live` — `axum::Json(Vec<LiveSessionBeat>)`, the
 * session-presence twin of `PresenceBeat` above (same crate, same hand-write
 * rationale — see that type's doc comment). Only `session_id` is consumed by
 * the machine lens's `liveSessionSet()` port (`lib/flow.ts`); the daemon
 * sends more fields (see `tests/parity/corpus/fleet-sessions-live.json`,
 * empty in the recorded corpus — no session was live at record time), widen
 * this interface if a future lens needs them.
 * Source: `crates/darkmux-flow/src/presence.rs`. */
export interface LiveSessionBeat {
  session_id: string;
}

/** A raw flow record — the JSONL shape every `/flow/<date>` entry and SSE
 * tail message carries (`darkmux_flow::FlowRecord`, `crates/darkmux-flow`).
 * Hand-written for the same reason as `PresenceBeat`: bridging it with ts-rs
 * would add ts-rs to a production-consumed lib crate, not just
 * `darkmux-serve`'s test-only surface. Only the fields the machine lens's
 * runs-list/health-region port (`lib/flow.ts`, ported from `viewer.html`'s
 * `flowToRenderModel`/`recentRow`/`dispatch*` functions) actually reads are
 * typed here — a flow record carries more (see any `tests/parity/corpus/
 * flow-*.json` fixture for the full shape); widen as a future lens needs
 * more of it. `payload` is intentionally loose (`Record<string, unknown>`)
 * — its shape varies per `action`, same as the legacy JS's untyped access.
 *
 * `fields` widened for `lib/flow.ts::flowToRenderModel` (the session
 * drill-in's data source, ported from viewer.html's own
 * `flowToRenderModel`) — a real telemetry record carries its type-specific
 * data under `fields` on the wire (schema 1.6+); `flowToRenderModel`
 * ALIASES `payload` onto `fields` for the (rarer, pre-1.6 or synthesized)
 * records that only carry the former, so a reader can always go through
 * `fields` uniformly, matching legacy's own `o.fields=o.payload` line. */
export interface FlowRecord {
  ts: string;
  level?: string;
  category?: string;
  tier?: string;
  stage?: string;
  action?: string;
  handle?: string;
  session_id?: string;
  source?: string;
  model?: string;
  mission_id?: string;
  phase_id?: string;
  machine_id?: string;
  machine_uid?: string;
  orchestrator?: string;
  payload?: Record<string, unknown>;
  fields?: Record<string, unknown>;
  /** Present only on the schema-header line every flow file leads with —
   * `flowToRenderModel`'s `!r._type` filter drops it before it reaches the
   * render model (see `lib/flow.ts::buildFlowWindow`). */
  _type?: string;
}

/** The subset of `dispatch.complete`/`dispatch.error`'s `payload` the
 * runs-list row (`recentRow()` in `viewer.html`) reads for its collapsed
 * summary line — the only state the `<details>` element's closed-by-default
 * `innerText` ever exposes (its `.rrdetail` expansion is hidden markup, out
 * of the parity harness's extraction target; see `tests/parity/README.md`).
 * `endpoint` widened for the session drill-in's `runRegions()` port
 * (`lenses/session/sessionRun.ts`) — the review path stamps the remote
 * endpoint only on the terminal payload, not on start (see that module's
 * own doc, ported from viewer.html:2131-2135). */
export interface DispatchCompletePayload {
  total_turns?: number;
  total_tools?: number;
  total_tokens?: number;
  total_compactions?: number;
  result_class?: string;
  exit_code?: number;
  prompt_tokens?: number;
  completion_tokens?: number;
  endpoint?: string;
}

/** The subset of `dispatch.start`'s `payload` the session drill-in's
 * `runRegions()` port (`lenses/session/sessionRun.ts`) reads for the brief
 * kv rows — viewer.html:2130 (`sp=(d&&d.payload)||{}`) onward. `prompt`
 * (the #1127 full-text field) is real but rarely emitted (see that
 * source's own comment: "the prompt TEXT is not emitted today (only
 * prompt_chars)") — both are typed since the source code branches on
 * `sp.prompt` truthy first. */
export interface DispatchStartPayload {
  runtime?: string;
  image?: string;
  workspace?: string;
  endpoint?: string;
  prompt?: string;
  prompt_chars?: number;
}

/** One `funnel-events.jsonl` line — the lab-run detail's event feed +
 * pipeline-stage source (`viewer.html`'s `computeLabPipeline`/
 * `renderLabFeed`, ported in `lenses/runs/labRun.ts`). Loosely typed
 * (`payload` is `Record<string, unknown>`, same posture as `FlowRecord`
 * above) since its shape varies per `step_id`/`action` — the pure logic
 * narrows what it needs per case, matching legacy's own untyped JS access.
 * Source: `funnel-events.jsonl` lines, written by
 * `crates/darkmux-lab/src/lab/review.rs`'s event emitters. */
export interface LabRunEvent {
  ts: string;
  action?: string;
  category?: string;
  source?: string;
  payload?: Record<string, unknown>;
}

/** `GET /lab/run/events?dir=&offset=` — the poll-based tail response.
 * Source: `crates/darkmux-serve/src/lib.rs::LabRunEventsResponse`. */
export interface LabRunEventsResponse {
  lines: LabRunEvent[];
  next_offset: number;
  finished: boolean;
}

/** The fields of `ReviewEnvelope` (`crates/darkmux-lab/src/lab/review.rs`)
 * `renderLabRun()`/`labCliHint()` actually read — the envelope carries far
 * more (members/steps/flags/judged/…), unread by this port; widen as a
 * future lab-run-detail feature needs more of it, per this file's own
 * convention. */
export interface LabFunnelEnvelope {
  crew: string;
  mode: string;
  confirmed: number;
  needs_check: number;
  archived: number;
}

/** The fields of `ScoresDoc.provenance` (`RunProvenance`,
 * `crates/darkmux-lab/src/lab/scores.rs`) `labCliHint()` reads
 * (`scores.crew`/`scores.exec_mode` in the legacy source — read off a
 * top-level `crew`/`exec_mode` the real `RunProvenance` doesn't actually
 * carry; see `lenses/runs/labRun.ts`'s own doc for why the legacy source
 * itself only ever hits the `env` half of this fallback in practice). Kept
 * minimal on purpose — `ScoresDoc.rows` (the real scoring output) is unread
 * by the lab-run-detail view this packet ports. */
export interface LabScoresDoc {
  crew?: string;
  exec_mode?: string;
}

/** `GET /lab/run/detail?dir=` — the envelope(s) + scores content for one
 * run. Source: `crates/darkmux-serve/src/lib.rs::LabRunDetailResponse`. */
export interface LabRunDetailResponse {
  dir: string;
  funnels: LabFunnelEnvelope[];
  scores: LabScoresDoc | null;
}
