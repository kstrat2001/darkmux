import { useMemo } from "react";
import { useQueries } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { queryKeys } from "../lib/queryKeys";
import { buildFlowWindow, computeTMax, prevDateUTC, todayUTC } from "../lib/flow";
import type { FlowRecord } from "../types/handwritten";

/** A `/flow/<date>` response body is EITHER a bare array or one of two
 * wrapper shapes — `loadLiveWindow()`, viewer.html:3502:
 * `Array.isArray(body)?body:(body.records||body.flow||[])`. The recorded
 * corpus is always a bare array; this stays loose for parity with the
 * legacy tolerance rather than assuming the shape never changes. */
function asRecordArray(body: unknown): FlowRecord[] {
  if (Array.isArray(body)) return body as FlowRecord[];
  if (body && typeof body === "object") {
    const obj = body as { records?: FlowRecord[]; flow?: FlowRecord[] };
    return obj.records ?? obj.flow ?? [];
  }
  return [];
}

export interface FlowWindowResult {
  /** True once BOTH day-fetches have settled (success or failure) — mirrors
   * `loadLiveWindow`'s await, not a per-query pending flag. */
  settled: boolean;
  data: FlowRecord[];
  tMax: number;
}

/** `loadLiveWindow()` (viewer.html:3497) as a query hook: fetches
 * `[prevDate, today]` (that exact order — see `lib/flow.ts`'s module doc)
 * and folds the result through `buildFlowWindow`. A day that 404s or
 * otherwise fails contributes no records — `loadLiveWindow`'s own `anyOk`
 * tolerance, not a hard error (the other day's records still render). */
export function useFlowWindow(nowMs: number): FlowWindowResult {
  const today = todayUTC();
  const yesterday = prevDateUTC(today);

  const results = useQueries({
    queries: [
      { queryKey: queryKeys.flowDate(yesterday), queryFn: () => fetchJson<unknown>(`/flow/${yesterday}`) },
      { queryKey: queryKeys.flowDate(today), queryFn: () => fetchJson<unknown>(`/flow/${today}`) },
    ],
  });

  const [yQuery, tQuery] = results;
  const settled = yQuery.status !== "pending" && tQuery.status !== "pending";

  const data = useMemo(() => {
    const yData = yQuery.data?.ok ? asRecordArray(yQuery.data.data) : [];
    const tData = tQuery.data?.ok ? asRecordArray(tQuery.data.data) : [];
    return buildFlowWindow(yData, tData, nowMs);
  }, [yQuery.data, tQuery.data, nowMs]);

  const tMax = useMemo(() => computeTMax(data), [data]);

  return { settled, data, tMax };
}
