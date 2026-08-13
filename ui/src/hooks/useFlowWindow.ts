import { useEffect, useMemo, useState } from "react";
import { skipToken, useQueries, useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { DATE_ROLLOVER_CHECK_MS, queryKeys } from "../lib/queryKeys";
import { asRecordArray, buildFlowWindow, computeTMax, prevDateUTC, todayUTC } from "../lib/flow";
import { isStaticBuild } from "../lib/staticSource";
import type { FlowRecord } from "../types/handwritten";

export interface FlowWindowResult {
  /** True once BOTH day-fetches have settled (success or failure) — mirrors
   * `loadLiveWindow`'s await, not a per-query pending flag. */
  settled: boolean;
  data: FlowRecord[];
  tMax: number;
}

/** `loadLiveWindow()` (viewer.html:3497) as a query hook: fetches
 * `[prevDate, today]` (that exact order — see `lib/flow.ts`'s module doc
 * for the fetch-order subtlety that makes the two-day merge order
 * load-bearing) and folds the result through `buildFlowWindow`. A day that
 * 404s or otherwise fails contributes no records — `loadLiveWindow`'s own
 * `anyOk` tolerance, not a hard error (the other day's records still
 * render).
 *
 * (Packet 5) ALSO folds in the live tail: `useLiveTail` (the App-level SSE +
 * reconcile-backstop hook, `hooks/useLiveTail.ts`) writes appended records
 * into `queryKeys.flowTail(date)` — a SEPARATE cache slot from this hook's
 * own `flowDate(date)` day-fetch (see `queryKeys.ts`'s own doc for why the
 * two stay apart). Subscribed here via `skipToken`, which reads the cache
 * reactively WITHOUT this hook ever triggering a fetch for that key itself
 * — only `useLiveTail` writes there, the same one-writer/many-reader shape
 * `RAW` has in legacy (`loadLiveWindow` seeds it, `startLiveTail`'s
 * `onmessage` plus `reconcileLiveWindow` both append onto it afterward, and
 * every render reads the same array). Every consumer of this hook
 * (`App.tsx`'s `#meta` line, `MachineLens`'s runs list) picks up live
 * records automatically — no consumer-side change needed — because
 * TanStack Query's cache is shared process-wide, not per-hook-instance. A
 * page where `useLiveTail` never mounted (impossible today — `App.tsx`
 * always mounts it — but relevant if a future packet route-gates it, see
 * `lib/route.ts`'s `isLiveRoute`) just sees an always-empty tail, which is
 * a silent, correct no-op here (matching legacy's playback mode, where
 * `RAW` is never appended to after the initial fetch either). */
export function useFlowWindow(nowMs: number): FlowWindowResult {
  // (QA, packet 5) The window OWNS its own rollover. It used to derive
  // `today` once per render and rely on something else re-rendering it at
  // midnight — which nothing reliably does. `useLiveTail` invalidating the
  // new day's query is a no-op (that query does not exist yet at the
  // rollover instant), so on an idle daemon the window kept yesterday's keys
  // indefinitely while the reopened stream wrote the new day's first records
  // into a cache slot nothing subscribed to: records arriving, and invisible.
  //
  // A cheap self-check on the same 5s cadence as the live poll fixes it at
  // the source. `setToday` only fires when the value actually changes, so a
  // steady day costs one string compare per tick and zero re-renders.
  const [today, setToday] = useState(todayUTC);
  useEffect(() => {
    const id = setInterval(() => {
      const now = todayUTC();
      setToday((prev) => (prev === now ? prev : now));
    }, DATE_ROLLOVER_CHECK_MS);
    return () => clearInterval(id);
  }, []);
  const yesterday = prevDateUTC(today);

  // (#1801) A daemon-less build has no `/flow/<date>` to fetch — legacy's
  // flow-src branch never calls `loadLiveWindow` at all (viewer.html:3897).
  // Without this gate the demo issues two guaranteed-404 requests on its
  // landing page, before any lens is touched (measured:
  // `/flow/2026-08-12`, `/flow/2026-08-13` in the page's resource timings).
  //
  // Gated on the BUILD, deliberately, not on `isLiveRoute(route)`. The window
  // still legitimately feeds machine-name lookups on a daemon-served playback
  // route (`App.tsx`'s `localMachineUid`/`nameOf`), and route-gating it would
  // silently change what those render — the shared-cache trap this arc has
  // already paid for twice. On a static build the fetch could only ever fail,
  // so suppressing it changes nothing a consumer can observe. The broader
  // question of route-gating this window is tracked separately as #1805.
  const daemonBacked = !isStaticBuild();

  const results = useQueries({
    queries: [
      {
        queryKey: queryKeys.flowDate(yesterday),
        queryFn: () => fetchJson<unknown>(`/flow/${yesterday}`),
        enabled: daemonBacked,
      },
      {
        queryKey: queryKeys.flowDate(today),
        queryFn: () => fetchJson<unknown>(`/flow/${today}`),
        enabled: daemonBacked,
      },
    ],
  });

  const [yQuery, tQuery] = results;
  // A disabled query sits at status `pending` forever, so the live-build
  // definition of "settled" would leave a static build permanently loading.
  const settled = !daemonBacked || (yQuery.status !== "pending" && tQuery.status !== "pending");

  // `queryFn: skipToken` — this hook never fetches these keys, only reads
  // whatever `useLiveTail` has (or hasn't yet) written there.
  const yTailQuery = useQuery<FlowRecord[]>({ queryKey: queryKeys.flowTail(yesterday), queryFn: skipToken });
  const tTailQuery = useQuery<FlowRecord[]>({ queryKey: queryKeys.flowTail(today), queryFn: skipToken });

  const data = useMemo(() => {
    const yData = yQuery.data?.ok ? asRecordArray(yQuery.data.data) : [];
    const tData = tQuery.data?.ok ? asRecordArray(tQuery.data.data) : [];
    const yMerged = yTailQuery.data?.length ? [...yData, ...yTailQuery.data] : yData;
    const tMerged = tTailQuery.data?.length ? [...tData, ...tTailQuery.data] : tData;
    return buildFlowWindow(yMerged, tMerged, nowMs);
  }, [yQuery.data, tQuery.data, yTailQuery.data, tTailQuery.data, nowMs]);

  const tMax = useMemo(() => computeTMax(data), [data]);

  return { settled, data, tMax };
}
