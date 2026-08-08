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

/** `GET /fleet/machines/live` — `axum::Json(Vec<PresenceBeat>)`, a REAL typed
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
  current_bytes: number;
  state: "green" | "yellow" | "red" | string;
}

/** `GET /lab/runs`'s per-seat staffing snapshot — only the fields the runs
 * lens's series/knob-diff view actually reads (`labKnobSummary`/
 * `labKnobDiff` in the legacy viewer). `SeatStaffingSnapshot` carries more
 * (`role_id`, `remote`, `endpoint`, `passes`, `selector`, `provenance`) —
 * widen this as a lens actually consumes more of it, per this file's own
 * convention. Source: `crates/darkmux-lab/src/lab/review.rs::SeatStaffingSnapshot`. */
export interface SeatStaffing {
  name: string;
  model: string;
  k: number;
  n_ctx?: number;
  max_tokens?: number;
}

/** Source: `crates/darkmux-lab/src/lab/review.rs::StaffingSnapshot` (the
 * `verify`/`request_changes` fields exist on the real struct but are unread
 * by the runs lens's series view — same widen-when-consumed note as above). */
export interface StaffingSnapshot {
  probes: SeatStaffing[];
  judge?: SeatStaffing;
}

/** One row of `GET /lab/runs`'s `runs` array — the SAME on-disk scan `/runs`
 * folds lab-bench runs from (see `Run`'s own doc), but carrying the lab-only
 * extras (`staffing` snapshot, bundle/flag counts) the flat `Run` shape
 * doesn't. Source: `crates/darkmux-serve/src/lib.rs::LabRunSummary`. */
export interface LabRun {
  dir: string;
  mtime_ms: number;
  case_ids: string[];
  crew?: string;
  exec_mode?: string;
  staffing?: StaffingSnapshot;
  bundles: number;
  raw_flags: number;
  deduped_flags: number;
  confirmed: number;
  needs_check: number;
  archived: number;
  degenerate: boolean;
  finished: boolean;
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

export interface MachineResources {
  schema_version: string;
  generated_at_ms: number;
  gather_ms: number;
  limit_bytes: number;
  limit_source: string;
  pool: { capacity_bytes: number; available_bytes: number };
  pressure: {
    swap_used_bytes: number;
    compressor_bytes: number;
    memory_free_percent: number;
    red: boolean;
  };
  models: MachineResourcesModel[];
  machine: {
    potential_bytes: number;
    unpriced_models: number;
    current_bytes: number;
    state: "green" | "yellow" | "red" | string;
  };
  attribution: string;
  attribution_note?: string;
  warnings: string[];
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
 * — its shape varies per `action`, same as the legacy JS's untyped access. */
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
  /** Present only on the schema-header line every flow file leads with —
   * `flowToRenderModel`'s `!r._type` filter drops it before it reaches the
   * render model (see `lib/flow.ts::buildFlowWindow`). */
  _type?: string;
}

/** The subset of `dispatch.complete`/`dispatch.error`'s `payload` the
 * runs-list row (`recentRow()` in `viewer.html`) reads for its collapsed
 * summary line — the only state the `<details>` element's closed-by-default
 * `innerText` ever exposes (its `.rrdetail` expansion is hidden markup, out
 * of the parity harness's extraction target; see `tests/parity/README.md`). */
export interface DispatchCompletePayload {
  total_turns?: number;
  total_tools?: number;
  total_tokens?: number;
  total_compactions?: number;
  result_class?: string;
  exit_code?: number;
  prompt_tokens?: number;
  completion_tokens?: number;
}
