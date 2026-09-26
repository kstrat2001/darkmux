import { isDispatchComplete } from "../../lib/flow";
import {
  CALL_KIND,
  hasAnyTokenCounts,
  runKeyMemo,
  sumUsage,
  type UsagePayload,
} from "../../lib/usageRecords";
import type { FlowRecord } from "../../types/handwritten";

/**
 * `tokensOffMeter()` — the fleet hero's numbers (#783, #1186, #1607, #2902).
 *
 * (#2902 step 2a) Every figure is a SUM OF FACTS from usage records, through
 * the one `sumUsage` (`lib/usageRecords.ts`): ALL TOKENS (`total_tokens`),
 * INPUT (`prompt_tokens`), GENERATED (`completion_tokens`), CACHED (the
 * provider's `cached_tokens`, over the records that report it; `null`, so no
 * chip, when none does) and UTILITY (the `purpose: utility` records:
 * darkmux's own compaction and radio routing). Legacy runs with no usage
 * records count their `dispatch complete` inside `sumUsage`; nothing here
 * special-cases them.
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
}

export function tokensOffMeter(data: FlowRecord[]): TokensOffMeter {
  const s = sumUsage(data);
  return { total: s.total, input: s.prompt, generated: s.completion, cached: s.cached, utility: s.utility, runs: dispatchCount(data) };
}

/**
 * The DISPATCHES chip: how many dispatches ran, from the dispatch bookends.
 * (#2902 step 2a) Behavior-identical to the run count #2659/#2709 settled
 * (measured: zero differences at every prefix of both parity corpora and the
 * demo replay); only the local/cloud labels it used to carry are gone.
 *
 * Keyed on `runKey`, `(session_id, mission_id)`: a deterministic session id
 * recurs across unrelated runs (#2709). Per run key:
 *
 *   - every token-bearing `dispatch complete` is one run (#2659: a re-launch
 *     under the same deterministic id closes with its own completion);
 *   - a token-LESS completion is one more run unless a token-bearing
 *     completion of the same LINEAGE already counted it. The lineage is
 *     whether the bookend names an `endpoint` (two seats sharing a
 *     task-scoped id, one on a named endpoint and one not, are two runs; a
 *     local `dispatch.map` step's token-less summary beside its seat's
 *     token-bearing completion is one, the MUST FIX 2 shape). This is a
 *     dedup of bookends, not a classification of where anything ran;
 *   - a key with no completion but with per-turn usage is one run in flight.
 *     Which usage records open a run is the pre-2a set (turn and map-item
 *     records with counts): a single-shot's record lands with its own
 *     completion, and a compactor call is not a dispatch.
 */
function dispatchCount(data: FlowRecord[]): number {
  // Per run key, a bit set over its completions (one pass, one key string
  // per relevant record): which lineages it closed on, and which of those
  // closed with a token-bearing completion.
  const WITH_EP = 1, WITHOUT_EP = 2, TOK_WITH_EP = 4, TOK_WITHOUT_EP = 8;
  const closed = new Map<string, number>();
  const inFlight = new Set<string>();
  const runKey = runKeyMemo();
  let runs = 0;
  for (const r of data) {
    const p = r.payload as (UsagePayload & { endpoint?: unknown }) | undefined;
    if (r.category === "telemetry" && r.source === "tokens") {
      const u = p ?? {};
      if (u.call_kind !== CALL_KIND.single_shot && u.call_kind !== CALL_KIND.compaction && u.token_source !== "absent") inFlight.add(runKey(r));
      continue;
    }
    if (!p || !r.session_id || !isDispatchComplete(r.action)) continue;
    const k = runKey(r);
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
