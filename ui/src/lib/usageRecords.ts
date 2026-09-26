/**
 * (#2902) The viewer's ONE token sum.
 *
 * Every model call darkmux makes emits exactly one `telemetry.tokens` usage
 * record (flow schema 1.57.0+), carrying `call_kind`, `purpose` (1.59.0+),
 * `requested_model`, `reported_model`, `endpoint`, `token_source` and the
 * provider's own counts. A token total anywhere in the viewer (the fleet
 * hero, the run page's tiles, the mission graph's step meter) is a PLAIN SUM
 * of those records, through `sumUsage` (or, for a fold that sees one record
 * at a time, its per-record half `usageContribution`). No run keying, no
 * complete-vs-telemetry precedence, no local/cloud classification, no
 * estimates: every figure is a sum of counts a provider reported.
 *
 * THE ONE EXCEPTION is legacy data, isolated in `isLegacyFallbackComplete`: a run
 * with ZERO usage records (written before flow schema 1.57.0, or by a fleet
 * peer on an older darkmux) counts each of its token-bearing
 * `dispatch complete` records once. A run with any usage record, even a
 * count-less `token_source: "absent"` one, never reads its complete.
 *
 * `purpose` and `call_kind` values come from the Rust enums
 * (`darkmux_crew::usage::{UsagePurpose, CallKind}`) through their generated
 * TS bindings; `PURPOSE`/`CALL_KIND` below are the only place the viewer
 * spells them, and `satisfies` fails the typecheck if either drifts from the
 * generated union.
 *
 * The shared golden fixture `tests/usage-golden/` (records + expected
 * totals) pins this module and step 2b's Rust aggregator to one answer.
 */

import type { CallKind } from "../types/generated/CallKind";
import type { UsagePurpose } from "../types/generated/UsagePurpose";
import { isDispatchComplete } from "./flow";

/** Every `UsagePurpose` variant, by name. A key missing or extra relative to
 *  the generated union is a type error. */
export const PURPOSE = { work: "work", utility: "utility" } as const satisfies { readonly [K in UsagePurpose]: K };

/** Every `CallKind` variant, by name. Same drift guard as `PURPOSE`. */
export const CALL_KIND = {
  turn: "turn",
  single_shot: "single_shot",
  map_item: "map_item",
  compaction: "compaction",
} as const satisfies { readonly [K in CallKind]: K };

/** The fields of a usage record's payload this module reads. Loose, like the
 *  wire: every field may be absent on an older record. */
export interface UsagePayload {
  call_kind?: unknown;
  purpose?: unknown;
  token_source?: unknown;
  total_tokens?: unknown;
  prompt_tokens?: unknown;
  completion_tokens?: unknown;
  cached_tokens?: unknown;
  /** The retired review path's spelling of its own spend, on a legacy
   *  `dispatch complete` only. */
  remote_tokens?: unknown;
}

/** A flow record as either shape the viewer holds it in: the wire's
 *  `payload`, or the render model's `fields`. */
export interface UsageRecordLike {
  ts?: string;
  action?: string;
  category?: string;
  source?: string;
  session_id?: string | null;
  mission_id?: string | null;
  handle?: string | null;
  machine_uid?: string | null;
  payload?: unknown;
  fields?: unknown;
}

function payloadOf(r: UsageRecordLike): UsagePayload {
  return ((r.payload ?? r.fields) as UsagePayload | null | undefined) ?? {};
}

function num(v: unknown): number {
  return typeof v === "number" && Number.isFinite(v) ? v : 0;
}

/** True for a usage record (`telemetry.tokens`), in either spelling the
 *  viewer receives it (category+source, or the action). */
export function isUsageRecord(r: UsageRecordLike): boolean {
  return (r.category === "telemetry" && r.source === "tokens") || r.action === "telemetry.tokens";
}

/** A record's `purpose`. Records from before flow schema 1.59.0 carry none;
 *  for those (THE LEGACY RULE, the only one) a compactor call is utility and
 *  anything else, a legacy `dispatch complete` included, is work. */
export function usagePurpose(p: UsagePayload | null | undefined): UsagePurpose {
  if (p?.purpose === PURPOSE.utility || p?.purpose === PURPOSE.work) return p.purpose;
  return p?.call_kind === CALL_KIND.compaction ? PURPOSE.utility : PURPOSE.work;
}

/** True for a runtime COMPACTOR call's usage record. */
export function isCompactionUsage(p: UsagePayload | null | undefined): boolean {
  return !!p && p.call_kind === CALL_KIND.compaction;
}

/** True for a per-TURN usage record: `call_kind` absent (every record from
 *  before step 1a is a turn or a map item keyed by no turn) or `turn`. The
 *  live rate and chars-per-token calibration pair a turn's tokens with that
 *  turn's own heartbeats, so a single-shot or map-item call must never land
 *  in a turn's bucket. */
export function isTurnUsage(p: UsagePayload | null | undefined): boolean {
  if (!p) return true;
  return p.call_kind === undefined || p.call_kind === CALL_KIND.turn;
}

/** One record's contribution to a sum. `cached` is `null` when the record
 *  does not report `cached_tokens` (never assumed zero). */
export interface UsageAmount {
  total: number;
  prompt: number;
  completion: number;
  cached: number | null;
  purpose: UsagePurpose;
}

export interface SumOptions {
  /** Leave out every record of this purpose. An execution's own numbers
   *  (run page tiles, mission-graph step meter) pass `PURPOSE.utility`:
   *  darkmux's utility jobs are sub-executions, never blended into the
   *  primary (CLAUDE.md contract 8). */
  exclude?: UsagePurpose;
}

function amountOf(p: UsagePayload, opts: SumOptions): UsageAmount | null {
  const purpose = usagePurpose(p);
  if (opts.exclude === purpose) return null;
  const prompt = num(p.prompt_tokens);
  const completion = num(p.completion_tokens);
  const total = num(p.total_tokens) || prompt + completion || num(p.remote_tokens);
  const cached = typeof p.cached_tokens === "number" && Number.isFinite(p.cached_tokens) ? p.cached_tokens : null;
  return { total, prompt, completion, cached, purpose };
}

/** The per-record half of `sumUsage`: what one usage record adds, or `null`
 *  when it is not a usage record or its purpose is excluded. The mission
 *  graph folds records one at a time and calls this directly. `total` is
 *  the provider's own total, falling back to prompt + completion only when
 *  the record reported a split without one (the writer's own precedence). */
export function usageContribution(r: UsageRecordLike, opts: SumOptions = {}): UsageAmount | null {
  if (!isUsageRecord(r)) return null;
  return amountOf(payloadOf(r), opts);
}

/** The identity of ONE RUN: `(session_id, mission_id)`. A bare session id
 *  is not one: `session_id::task`/`mission_run` are deterministic, so the
 *  same id recurs across unrelated runs (#2690/#2709). The legacy fallback
 *  and the hero's run count both key on this. A sessionless record gets a
 *  composite of its own; `\u0000` cannot occur inside either id. */
export function runKey(r: UsageRecordLike): string {
  const sid = r.session_id || `ts:${r.ts}:${r.handle || ""}:${r.machine_uid || ""}`;
  return `${sid}\u0000${r.mission_id || ""}`;
}

/** A `runKey` that reuses the previous key when consecutive records name the
 *  same `(session_id, mission_id)` (a run's turns arrive together), so a
 *  pass over a window builds one key string per run instead of one per
 *  record. Same result as `runKey`, always. */
export function runKeyMemo(): (r: UsageRecordLike) => string {
  let lastSid: string | null | undefined;
  let lastMid: string | null | undefined;
  let lastKey = "";
  return (r) => {
    if (!r.session_id) return runKey(r);
    if (r.session_id !== lastSid || r.mission_id !== lastMid) {
      lastSid = r.session_id;
      lastMid = r.mission_id;
      lastKey = runKey(r);
    }
    return lastKey;
  };
}

/** True when a `dispatch complete`'s payload carries any token count. */
export function hasAnyTokenCounts(p: UsagePayload): boolean {
  return !!(num(p.total_tokens) || num(p.prompt_tokens) || num(p.completion_tokens) || num(p.remote_tokens));
}

/** THE LEGACY FALLBACK, and the only exception to the plain sum: a
 *  token-bearing `dispatch complete` counts (once) when its run key holds
 *  ZERO usage records. Before flow schema 1.57.0 a single-shot or hosted
 *  call emitted no usage record, so its complete was the only place its
 *  tokens were written; a run with any usage record (even an `absent` one)
 *  is fully described by its records and its complete is never read.
 *  `runsWithUsage` is the set of run keys that hold a usage record. */
function isLegacyFallbackComplete(r: UsageRecordLike, runsWithUsage: ReadonlySet<string>, key: (r: UsageRecordLike) => string): boolean {
  return isDispatchComplete(r.action) && hasAnyTokenCounts(payloadOf(r)) && !runsWithUsage.has(key(r));
}

/** The records the legacy fallback counts, for a reader that needs them
 *  itself (a per-field breakdown). `sumUsage` applies the same rule. */
export function legacyCompleteCounts<R extends UsageRecordLike>(records: readonly R[]): R[] {
  const withUsage = new Set<string>();
  for (const r of records) if (isUsageRecord(r)) withUsage.add(runKey(r));
  return records.filter((r) => isLegacyFallbackComplete(r, withUsage, runKey));
}

/** A mission-graph step's token figure: its usage records' sum once any
 *  usage record for it has been seen, else (legacy) the finalized total its
 *  `dispatch complete`/`step result` reported (or the server backfilled).
 *  The same legacy rule as `isLegacyFallbackComplete`, at the grain the
 *  graph folds: a step, not a run key. */
export function stepTokensWithLegacyFallback(usageSum: number, usageRecordsSeen: boolean, completeTotal: number): number {
  return usageRecordsSeen ? usageSum : completeTotal;
}

export interface UsageSum {
  total: number;
  prompt: number;
  completion: number;
  /** Sum of `cached_tokens` over the records that report it; `null` when
   *  none does (the hero then shows no CACHED chip rather than a 0 nobody
   *  measured). */
  cached: number | null;
  /** Sum of `total` over `purpose: utility` records (0 when excluded). */
  utility: number;
  /** Usage records counted (after `exclude`), `absent` ones included. */
  usageRecords: number;
  /** Counted entries (usage records or legacy completes) that reported a
   *  count: `0` means nothing measured, which a tile shows as "—", never 0. */
  reported: number;
  /** Legacy `dispatch complete` records counted (after `exclude`). */
  legacyCompletes: number;
}

/** THE sum. Every usage record in `records` (minus `opts.exclude`), plus the
 *  legacy fallback's completes (`isLegacyFallbackComplete`). Linear in
 *  `records`, allocation-free per record: the hero recomputes it on every
 *  playback scrub. */
export function sumUsage(records: readonly UsageRecordLike[], opts: SumOptions = {}): UsageSum {
  const out: UsageSum = { total: 0, prompt: 0, completion: 0, cached: null, utility: 0, usageRecords: 0, reported: 0, legacyCompletes: 0 };
  const exclude = opts.exclude;
  const add = (p: UsagePayload): boolean => {
    const purpose = usagePurpose(p);
    if (exclude === purpose) return false;
    const prompt = num(p.prompt_tokens);
    const completion = num(p.completion_tokens);
    const total = num(p.total_tokens) || prompt + completion || num(p.remote_tokens);
    out.total += total;
    out.prompt += prompt;
    out.completion += completion;
    if (typeof p.cached_tokens === "number" && Number.isFinite(p.cached_tokens)) out.cached = (out.cached ?? 0) + p.cached_tokens;
    if (purpose === PURPOSE.utility) out.utility += total;
    if (hasAnyTokenCounts(p)) out.reported++;
    return true;
  };
  const withUsage = new Set<string>();
  const completes: UsageRecordLike[] = [];
  const key = runKeyMemo();
  for (const r of records) {
    if (isUsageRecord(r)) {
      withUsage.add(key(r));
      if (add(payloadOf(r))) out.usageRecords++;
    } else if (isDispatchComplete(r.action)) {
      completes.push(r);
    }
  }
  for (const r of completes) if (isLegacyFallbackComplete(r, withUsage, runKey) && add(payloadOf(r))) out.legacyCompletes++;
  return out;
}

/** (#2902 step 1b) True when a record's `handle` names the execution the
 *  record belongs to. A compactor call's usage record is attributed to the
 *  compactor (`handle: "compactor"`), a sub-execution INSIDE the session, so
 *  it never names the session's own role (a header saying who ran a session
 *  must not lose its role because the run compacted). */
export function handleNamesExecution(r: { action?: string; payload?: unknown; fields?: unknown }): boolean {
  if (r.action !== "telemetry.tokens") return true;
  const p = (r.payload ?? r.fields) as UsagePayload | null | undefined;
  return !isCompactionUsage(p);
}
