import {
  CALL_KIND,
  hasAnyTokenCounts,
  sumUsage,
  type UsagePayload,
} from "../../lib/usageRecords";
import { ACTION, CATEGORY, SOURCE, executionOf, type NormRecord } from "../../lib/ingest";

/**
 * `tokensOffMeter()` — the fleet hero's numbers (#783, #1186, #1607, #2902).
 *
 * (#2902 step 2a) Every figure is a SUM OF FACTS from usage records, through
 * the one `sumUsage` (`lib/usageRecords.ts`): ALL TOKENS (`total_tokens`),
 * INPUT (`prompt_tokens`), GENERATED (`completion_tokens`), CACHED (the
 * provider's `cached_tokens`, over the records that report it; `null`, so no
 * chip, when none does) and UTILITY (the `purpose: utility` records:
 * darkmux's own compaction and radio routing). A `dispatch complete` carries
 * no tokens in the sum (a record with no usage record reads as unmeasured).
 *
 * GONE, deliberately: the local/cloud/unknown split and its run counts
 * (withdrawn in #2834: an endpoint says what was called, never where or at
 * what cost), and the FRESH INPUT / RE-READ / UNCLASSIFIED tiles (an
 * estimate from turn order, not a count any provider reported). Where a
 * provider's total exceeds prompt + completion (reasoning counted
 * separately), ALL TOKENS is larger than INPUT + GENERATED and no filler
 * chip is invented for the difference.
 *
 * Playhead: this function takes no playhead. `FleetLens` filters `data` to
 * `ts <= playhead` before calling it (#1869), so every record here is
 * already visible as of the playhead.
 *
 * TOKENS ONLY, never a currency figure.
 */
export interface TokensOffMeter {
  total: number;
  input: number;
  generated: number;
  /** `null` when no record in the window reports `cached_tokens`. */
  cached: number | null;
  utility: number;
  runs: number;
  /** (#3067) The part of `total` on no run: usage records naming no session
   *  and no mission (radio routing, a probe). Every figure above counts them;
   *  no run's TOKENS cell does. The twin of `/runs`' `no_run`. */
  noRun: { calls: number; tokens: number };
  /** (#3067) The part of `total` that names a run with no row in the listing
   *  (a start record outside the window). Zero when no listing was given. */
  unlisted: { calls: number; tokens: number };
}

export function tokensOffMeter(data: NormRecord[], runIds?: ReadonlySet<string>): TokensOffMeter {
  const s = sumUsage(data);
  return { total: s.total, input: s.prompt, generated: s.completion, cached: s.cached, utility: s.utility, runs: dispatchCount(data), noRun: noRunUsage(data), unlisted: unlistedUsage(data, runIds) };
}

/** The usage records that name a session or mission no row of `runIds` is,
 *  the twin of the run build's `unlisted`. Nothing without a listing. */
export function unlistedUsage(data: readonly NormRecord[], runIds?: ReadonlySet<string>): { calls: number; tokens: number } {
  if (!runIds) return { calls: 0, tokens: 0 };
  const s = sumUsage(data.filter((r) => (r.session_id || r.mission_id) && !runIds.has(r.session_id ?? "") && !runIds.has(r.mission_id ?? "")));
  return { calls: s.usageRecords, tokens: s.total };
}

/** The usage records on no run: neither a session nor a mission named, the
 *  rule `usage_sum`'s `no_run` applies. */
export function noRunUsage(data: readonly NormRecord[]): { calls: number; tokens: number } {
  const s = sumUsage(data.filter((r) => !r.session_id && !r.mission_id));
  return { calls: s.usageRecords, tokens: s.total };
}

/**
 * The DISPATCHES chip: how many dispatches ran, from the dispatch bookends.
 * (#2902 step 2a) Behavior-identical to the run count #2659/#2709 settled
 * (measured: zero differences at every prefix of both parity corpora and the
 * demo replay); only the local/cloud labels it used to carry are gone. That
 * claim holds for numeric, `null` and absent token fields, which is every
 * value a producer writes. A NON-numeric count (`true`, `"7"`) now reads as
 * not-reported where the old count coerced it truthy — hardening, since no
 * producer writes one.
 *
 * Keyed on the execution (`executionOf`: the record's `execution_id`; a
 * pre-4.0 record names none and is keyed by its session and mission, since a
 * deterministic session id recurs across unrelated runs, #2709). Per
 * execution:
 *
 *   - every token-bearing `dispatch complete` is one run (#2659: a re-launch
 *     under the same deterministic id closes with its own completion);
 *   - a token-LESS completion is one more run unless a token-bearing
 *     completion of the same LINEAGE already counted it. The lineage is
 *     whether the bookend names an `endpoint` (two seats sharing a
 *     task-scoped id, one on a named endpoint and one not, are two runs; a
 *     pre-4.0 local `dispatch.map` step's token-less summary beside its seat's
 *     token-bearing completion is one, the MUST FIX 2 shape). This is a
 *     dedup of bookends, not a classification of where anything ran;
 *   - a key with no completion but with per-turn usage is one run in flight.
 *     Which usage records open a run is the pre-2a set (turn and map-item
 *     records with counts): a single-shot's record lands with its own
 *     completion, and a compactor call is not a dispatch.
 */
function dispatchCount(data: NormRecord[]): number {
  // Per execution, a bit set over its completions (one pass, one key string
  // per relevant record): which lineages it closed on, and which of those
  // closed with a token-bearing completion.
  const WITH_EP = 1, WITHOUT_EP = 2, TOK_WITH_EP = 4, TOK_WITHOUT_EP = 8;
  const closed = new Map<string, number>();
  const inFlight = new Set<string>();
  let runs = 0;
  for (const r of data) {
    const p = r.payload as (UsagePayload & { endpoint?: unknown }) | undefined;
    if (r.category === CATEGORY.Telemetry && r.source === SOURCE.Tokens) {
      const u = p ?? {};
      if (u.call_kind !== CALL_KIND.single_shot && u.call_kind !== CALL_KIND.compaction && u.token_source !== "absent") inFlight.add(executionOf(r));
      continue;
    }
    if (!p || !r.session_id || r.action !== ACTION.DispatchComplete) continue;
    const k = executionOf(r);
    const ep = !!p.endpoint;
    let bits = (closed.get(k) ?? 0) | (ep ? WITH_EP : WITHOUT_EP);
    if (hasAnyTokenCounts(p)) {
      runs++;
      bits |= ep ? TOK_WITH_EP : TOK_WITHOUT_EP;
    }
    closed.set(k, bits);
  }
  for (const bits of closed.values()) {
    if (bits & WITH_EP && !(bits & TOK_WITH_EP)) runs++;
    if (bits & WITHOUT_EP && !(bits & TOK_WITHOUT_EP)) runs++;
  }
  for (const k of inFlight) if (!closed.has(k)) runs++;
  return runs;
}
