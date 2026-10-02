/**
 * (#2902) The viewer's ONE token sum.
 *
 * Every model call darkmux makes emits exactly one `telemetry.tokens` usage
 * record (flow schema 1.57.0+), carrying `call_kind`, `purpose` (1.59.0+),
 * `requested_model`, `reported_model`, `endpoint`, `token_source` and the
 * provider's own counts. A token total anywhere in the viewer (the fleet
 * hero, the run page's tiles, the mission graph's step meter) is a PLAIN SUM
 * of those records, through `sumUsage` (or, for a fold that sees one record
 * at a time, its per-record half `usageContribution`). No execution keying, no
 * complete-vs-telemetry precedence, no local/cloud classification, no
 * estimates: every figure is a sum of counts a provider reported.
 *
 * There is no exception: a `dispatch complete` carries no tokens here,
 * whatever its payload says (5.0 dropped the pre-1.57 fallback to it). A run's
 * figure is every usage record of its session or mission, utility calls
 * included, with the utility part named (`UsageSum.utility`); the Rust fold
 * (`usage_sum.rs`) is the same sum and the server's `Run.tokens` /
 * `tokensFinal` are its answers.
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
import { ACTION, CATEGORY, SOURCE, type NormRecord } from "./ingest";

/** Every `UsagePurpose` variant a writer produces, by name. A key missing or
 *  extra relative to the generated union is a type error; `unknown` is the
 *  reader's word for a value from another build, never written. */
export const PURPOSE = { work: "work", utility: "utility" } as const satisfies { readonly [K in Exclude<UsagePurpose, "unknown">]: K };

/** Every `CallKind` variant, by name. Same drift guard as `PURPOSE`. */
export const CALL_KIND = {
  turn: "turn",
  single_shot: "single_shot",
  map_item: "map_item",
  compaction: "compaction",
} as const satisfies { readonly [K in Exclude<CallKind, "unknown">]: K };

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
}

/** A record's data, in either place it is held: the wire's `payload`, or
 *  the render model's `fields`. */
function payloadOf(r: NormRecord): UsagePayload {
  return ((r.payload ?? r.fields) as UsagePayload | null | undefined) ?? {};
}

/** The largest count either side holds exactly: 2^53, the edge of a JS
 *  number's integer range. The Rust twin (`usage_sum::MAX_COUNT`) clamps to
 *  the same edge, so a sum of clamped counts is one arithmetic on both
 *  sides. */
const MAX_COUNT = 2 ** 53;

/** THE value domain, shared with the Rust twin's `num`: a finite number is
 *  floored to an integer and clamped to [0, MAX_COUNT]; anything else (a
 *  string, a bool, null, a negative) reads as 0. "Reported" is judged by
 *  this same reading everywhere, so a negative count is not a count. */
function num(v: unknown): number {
  return typeof v === "number" && Number.isFinite(v) && v > 0 ? Math.min(Math.floor(v), MAX_COUNT) : 0;
}

/** The presence test for `cached_tokens`: a finite number at all (a
 *  reported `-3` is a reported 0, not an absence). */
function isFiniteNumber(v: unknown): v is number {
  return typeof v === "number" && Number.isFinite(v);
}

/** True for a usage record (`telemetry.tokens`), in either shape the viewer
 *  receives it (category+source, or the action). */
export function isUsageRecord(r: NormRecord): boolean {
  return (r.category === CATEGORY.Telemetry && r.source === SOURCE.Tokens) || r.action === ACTION.TelemetryTokens;
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

/** True for a usage record of ONE single-shot WORK call (a relayed or
 *  one-shot dispatch, which has no per-turn records). */
export function isSingleShotWorkUsage(r: NormRecord): boolean {
  const p = payloadOf(r);
  return isUsageRecord(r) && p.call_kind === CALL_KIND.single_shot && usagePurpose(p) === PURPOSE.work;
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
  const total = num(p.total_tokens) || prompt + completion;
  const cached = isFiniteNumber(p.cached_tokens) ? num(p.cached_tokens) : null;
  return { total, prompt, completion, cached, purpose };
}

/** The per-record half of `sumUsage`: what one usage record adds, or `null`
 *  when it is not a usage record or its purpose is excluded. The mission
 *  graph folds records one at a time and calls this directly. `total` is
 *  the provider's own total, falling back to prompt + completion only when
 *  the record reported a split without one (the writer's own precedence). */
export function usageContribution(r: NormRecord, opts: SumOptions = {}): UsageAmount | null {
  if (!isUsageRecord(r)) return null;
  return amountOf(payloadOf(r), opts);
}

/** True when a usage record's payload carries any token count. */
export function hasAnyTokenCounts(p: UsagePayload): boolean {
  return !!(num(p.total_tokens) || num(p.prompt_tokens) || num(p.completion_tokens));
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
  /** Usage records that reported a count: `0` means nothing measured, which
   *  a tile shows as "—", never 0. */
  reported: number;
}

/** THE sum. Every usage record in `records` (minus `opts.exclude`). Linear in
 *  `records`, one small `amountOf` object per record: the hero recomputes it
 *  on every playback scrub. The per-record arithmetic lives in `amountOf`
 *  alone; this only adds its results up. */
export function sumUsage(records: readonly NormRecord[], opts: SumOptions = {}): UsageSum {
  const out: UsageSum = { total: 0, prompt: 0, completion: 0, cached: null, utility: 0, usageRecords: 0, reported: 0 };
  for (const r of records) {
    if (!isUsageRecord(r)) continue;
    const p = payloadOf(r);
    const amount = amountOf(p, opts);
    if (!amount) continue;
    out.total += amount.total;
    out.prompt += amount.prompt;
    out.completion += amount.completion;
    if (amount.cached !== null) out.cached = (out.cached ?? 0) + amount.cached;
    if (amount.purpose === PURPOSE.utility) out.utility += amount.total;
    if (hasAnyTokenCounts(p)) out.reported++;
    out.usageRecords++;
  }
  return out;
}

/** (#2902 step 1b) True when a record's `handle` names the execution the
 *  record belongs to. A compactor call's usage record is attributed to the
 *  compactor (`handle: "compactor"`), a sub-execution INSIDE the session, so
 *  it never names the session's own role (a header saying who ran a session
 *  must not lose its role because the run compacted). */
export function handleNamesExecution(r: NormRecord): boolean {
  if (r.action !== ACTION.TelemetryTokens) return true;
  const p = (r.payload ?? r.fields) as UsagePayload | null | undefined;
  return !isCompactionUsage(p);
}
