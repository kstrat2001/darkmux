import { useEffect, useMemo, useState } from "react";
import { skipToken, useQueries, useQuery } from "@tanstack/react-query";
import { fetchJson, type FetchResult } from "../lib/fetcher";
import { DATE_ROLLOVER_CHECK_MS, RECONCILE_BACKSTOP_MS, queryKeys } from "../lib/queryKeys";
import { buildFlowWindow, computeTMax, prevDateUTC, todayUTC } from "../lib/flow";
import { getSource } from "../lib/source";
import { ingest, type NormRecord } from "../lib/ingest";

export interface FlowWindowResult {
  /** True once BOTH day-fetches have settled (success or failure) — mirrors
   * `loadLiveWindow`'s await, not a per-query pending flag. */
  settled: boolean;
  data: NormRecord[];
  tMax: number;
  /** (#2965) A day's read failed, or `null` when every day answered. A
   *  failed day settles the window and contributes no records, exactly as a
   *  quiet day does, so this is the only thing that tells the two apart: a
   *  negative claim ("idle") backed by a failed read is a claim nothing read.
   *  A day with no file is not a failure (the daemon answers it `200 []`).
   *  Current, not latched: a failed day retries every
   *  `RECONCILE_BACKSTOP_MS`, and its first success clears this. */
  failure: FlowReadFailure | null;
}

/** (#2965) What failed: the first failed day's status and message, and which
 *  of the window's two days failed. */
export interface FlowReadFailure {
  status: number | null;
  message: string;
  today: boolean;
  yesterday: boolean;
}

/** (#2965) Whether a day's answer is a FAILED read, as opposed to an empty
 *  day. Only 404 is an empty day: the daemon answers a missing day `200 []`,
 *  but a static host (the published demo, the e2e and parity harnesses)
 *  answers 404. Every other non-OK answer is a failure: a network error
 *  (`status: null`), a 5xx, a body that did not parse, and any other 4xx,
 *  which says the records exist and were refused. The daemon answers 401 to a
 *  remote read without the serve token (#881); reading that as an empty day
 *  would show "idle" to a viewer who was never shown the records. */
export function isFailedRead(r: FetchResult<unknown> | undefined): r is Extract<FetchResult<unknown>, { ok: false }> {
  if (!r || r.ok) return false;
  return r.status !== 404;
}

/** (#2911) How finely the flow window's trailing edge follows the clock. */
export const FLOW_WINDOW_EDGE_GRAIN_MS = 60_000;

/** The `nowMs` the window merge actually keys on: `nowMs` floored to
 *  `FLOW_WINDOW_EDGE_GRAIN_MS`, so every render inside one grain reuses the
 *  same merged array (see the `data` memo in `useFlowWindow`). */
export function flowWindowEdgeMs(nowMs: number): number {
  return Math.floor(nowMs / FLOW_WINDOW_EDGE_GRAIN_MS) * FLOW_WINDOW_EDGE_GRAIN_MS;
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
  const daemonBacked = getSource().kind === "daemon";

  // (#2965 review) A failed day retries itself. Nothing else refetches a
  // `flowDate` key: the live tail writes `flowTail`, focus refetch is off,
  // and `fetchJson` never throws, so TanStack's own retry never fires. Without
  // this a single failed read held the page at "no signal" until a reload.
  // A healthy day is never polled (`false`): the tail keeps it current.
  const retryFailed = (q: { state: { data?: FetchResult<unknown> } }) =>
    isFailedRead(q.state.data) ? RECONCILE_BACKSTOP_MS : false;
  const results = useQueries({
    queries: [
      {
        queryKey: queryKeys.flowDate(yesterday),
        queryFn: () => fetchJson<unknown>(`/flow/${yesterday}`),
        enabled: daemonBacked,
        refetchInterval: retryFailed,
      },
      {
        queryKey: queryKeys.flowDate(today),
        queryFn: () => fetchJson<unknown>(`/flow/${today}`),
        enabled: daemonBacked,
        refetchInterval: retryFailed,
      },
    ],
  });

  const [yQuery, tQuery] = results;
  // A disabled query sits at status `pending` forever, so the live-build
  // definition of "settled" would leave a static build permanently loading.
  const settled = !daemonBacked || (yQuery.status !== "pending" && tQuery.status !== "pending");

  // `queryFn: skipToken` — this hook never fetches these keys, only reads
  // whatever `useLiveTail` has (or hasn't yet) written there.
  const yTailQuery = useQuery<NormRecord[]>({ queryKey: queryKeys.flowTail(yesterday), queryFn: skipToken });
  const tTailQuery = useQuery<NormRecord[]>({ queryKey: queryKeys.flowTail(today), queryFn: skipToken });

  // (#2911) The merge keys on the window's trailing EDGE, not on `nowMs`
  // itself. Callers pass a fresh `Date.now()` every render, and the fleet
  // lens renders once a second while an execution is live, so keying on the
  // raw value re-ran the copy + normalize + sort + dedup of the whole window
  // every second for an edge nobody can see move: a 24h window's edge needs
  // minute precision at most. Floored, so the window keeps a record up to
  // one grain longer, never drops one early. A new record still lands at
  // once: it arrives as new query data, which is a dependency here.
  const windowEdgeMs = flowWindowEdgeMs(nowMs);
  const data = useMemo(() => {
    const yData = yQuery.data?.ok ? ingest(yQuery.data.data) : [];
    const tData = tQuery.data?.ok ? ingest(tQuery.data.data) : [];
    const yMerged = yTailQuery.data?.length ? [...yData, ...yTailQuery.data] : yData;
    const tMerged = tTailQuery.data?.length ? [...tData, ...tTailQuery.data] : tData;
    return buildFlowWindow(yMerged, tMerged, windowEdgeMs);
  }, [yQuery.data, tQuery.data, yTailQuery.data, tTailQuery.data, windowEdgeMs]);

  const tMax = useMemo(() => computeTMax(data), [data]);

  // (#2965) `fetchJson` never throws, so a failed read is a SUCCESSFUL query
  // carrying `ok: false`, the same shape `RunsBoard` and the presence
  // coverage read.
  const yFail = isFailedRead(yQuery.data) ? yQuery.data : null;
  const tFail = isFailedRead(tQuery.data) ? tQuery.data : null;
  const first = tFail ?? yFail;
  const failure = useMemo<FlowReadFailure | null>(
    () => (first ? { status: first.status, message: first.message, today: tFail !== null, yesterday: yFail !== null } : null),
    [first, tFail, yFail],
  );

  return { settled, data, tMax, failure };
}
