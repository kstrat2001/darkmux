import { useEffect, useState } from "react";
import { useQueryClient, type QueryClient } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS } from "../lib/queryKeys";
import { asRecordArray, mergeTailRecords, prevDateUTC, todayUTC, LIVE_WINDOW_MS } from "../lib/flow";
import { startFlowTail, type FlowTailHandle } from "../lib/sse";
import type { FlowRecord } from "../types/handwritten";

/**
 * Port of `viewer.html`'s live-tail wiring — `startLiveTail` (3587-3627),
 * `reconcileLiveWindow` (3758-3782), and `startLivePoll`'s date-rollover
 * detection (3783-3806) — as ONE hook, mounted once at `App.tsx`'s root
 * (the same App-level scope `useFlowWindow`/`useLiveMachines` already run
 * at). Owns:
 *
 * 1. The SSE tail itself (`lib/sse.ts::startFlowTail`, extended this packet
 *    with the `onOpen`/`onError` handlers this hook drives its status from).
 * 2. The ~20s reconcile backstop (every 4th 5s tick — `RECONCILE_BACKSTOP_MS`
 *    in `queryKeys.ts`, recorded there since Packet 1.5 for this packet to
 *    pick up).
 * 3. UTC date-rollover: closes the old day's stream, invalidates the
 *    `flowDate` queries for the new [prevDate, today] pair (so
 *    `useFlowWindow`'s own `useQueries` refetch fresh content — the
 *    `loadLiveWindow(nd)` half of legacy's rollover handler), and reopens
 *    the tail against the new day.
 *
 * Every record this hook appends (via SSE `onmessage`) or backfills (via
 * reconcile) lands in `queryKeys.flowTail(date)` — a cache slot
 * `useFlowWindow` reads (via `skipToken`, never fetches) and merges into its
 * own two-day window. This hook never touches `flowDate` directly except to
 * INVALIDATE it on rollover; it doesn't own that cache's content, only the
 * tail's.
 *
 * Not gated by `enabled` internally — the caller (`App.tsx`) passes
 * `isLiveRoute(route)` so the tail only runs on a genuinely live route (see
 * `lib/route.ts`'s own doc for why `playback`/`session`/`mission-redirect`
 * are excluded, mirroring legacy's `wantsPlayback` gate on `startLiveTail`).
 */
export type LiveTailStatus = "live" | "reconnecting";

/** `RECONCILE_OVERLAP_MS` — viewer.html:3385. Safety margin the reconcile
 * backstop's `?since=` subtracts from the newest record already held, so a
 * brief reconnect gap is still caught without re-pulling the whole day. */
const RECONCILE_OVERLAP_MS = 30 * 60 * 1000;

/** Injectable seams for testing — none of these are ever passed in
 * production (`App.tsx` calls `useLiveTail(enabled)` with no second arg),
 * matching `lib/sse.ts`'s own factory-injection precedent. */
export interface UseLiveTailDeps {
  eventSourceFactory?: (url: string) => EventSource;
  fetchImpl?: typeof fetchJson;
  /** Overrides the 5s ticker (`PRESENCE_POLL_MS`) — tests use this to avoid
   * waiting on real wall-clock intervals. */
  tickMs?: number;
}

/** `nd!==LIVE_ES_DATE` viewer.html:3792's reload half —
 * `loadLiveWindow(nd)`'s effect achieved here by INVALIDATING the two
 * `flowDate` queries `useFlowWindow` owns, rather than fetching here too and
 * risking a second, differently-shaped write into a cache slot this hook
 * doesn't own. */
function reloadWindowForNewDay(queryClient: QueryClient, newToday: string): void {
  queryClient.invalidateQueries({ queryKey: queryKeys.flowDate(newToday) });
  queryClient.invalidateQueries({ queryKey: queryKeys.flowDate(prevDateUTC(newToday)) });
}

/** `reconcileLiveWindow()` — viewer.html:3758-3782. Fetches
 * `/flow/<d>?since=<newest held - overlap>` for `[prevDate(date), date]`
 * and merges anything new into that day's `flowTail` cache slot, deduped +
 * windowed via `mergeTailRecords` (this hook's `SEEN_KEYS` analog). */
async function reconcile(
  queryClient: QueryClient,
  date: string,
  fetchImpl: typeof fetchJson,
): Promise<void> {
  const cutMs = Date.now() - LIVE_WINDOW_MS;
  for (const d of [prevDateUTC(date), date]) {
    const tailKey = queryKeys.flowTail(d);
    const dayKey = queryKeys.flowDate(d);
    const existingTail = queryClient.getQueryData<FlowRecord[]>(tailKey) ?? [];
    const dayResult = queryClient.getQueryData<{ ok: boolean; data?: unknown }>(dayKey);
    const existingDay = dayResult && dayResult.ok ? asRecordArray(dayResult.data) : [];
    const held = [...existingDay, ...existingTail];
    const newest = held.reduce((m, r) => {
      const t = r?.ts ? Date.parse(r.ts) : NaN;
      return Number.isFinite(t) && t > m ? t : m;
    }, 0);
    const sinceMs = newest ? Math.max(cutMs, newest - RECONCILE_OVERLAP_MS) : cutMs;
    const sinceIso = new Date(sinceMs).toISOString().replace(/\.\d+Z$/, "Z");
    let res;
    try {
      res = await fetchImpl<unknown>(`/flow/${d}?since=${encodeURIComponent(sinceIso)}`);
    } catch {
      continue; // transient — the next tick retries, same as legacy's try/catch-per-day
    }
    if (!res.ok) continue;
    const recs = asRecordArray(res.data);
    if (!recs.length) continue;
    queryClient.setQueryData<FlowRecord[]>(tailKey, (prev) => mergeTailRecords(prev ?? [], recs, cutMs));
  }
}

export function useLiveTail(enabled: boolean, deps: UseLiveTailDeps = {}): LiveTailStatus {
  const queryClient = useQueryClient();
  const [status, setStatus] = useState<LiveTailStatus>("live");
  const { eventSourceFactory, fetchImpl, tickMs } = deps;

  useEffect(() => {
    if (!enabled) return;
    // `typeof EventSource==="undefined"` — viewer.html:3588's own guard.
    // jsdom (this app's unit-test environment) has no `EventSource` global;
    // a test-injected `eventSourceFactory` bypasses the check, same as
    // `sse.test.ts`'s own MockEventSource pattern.
    const canStream = typeof EventSource !== "undefined" || !!eventSourceFactory;
    const doFetch = fetchImpl ?? fetchJson;

    let cancelled = false;
    let tailDate = todayUTC();
    let everOpened = false;
    let handle: FlowTailHandle | null = null;

    const openTail = (date: string) => {
      handle = startFlowTail(queryClient, queryKeys.flowTail(date), date, eventSourceFactory, {
        onOpen: () => {
          if (cancelled) return;
          setStatus("live");
          // (#1480 part 1) Self-heal on RECONNECT (not the initial connect)
          // — a reconnected EventSource tails from NOW, silently dropping
          // whatever was emitted during the gap. Reconcile pulls it back in.
          if (everOpened) void reconcile(queryClient, date, doFetch);
          everOpened = true;
        },
        onError: () => {
          if (cancelled) return;
          // (#1480 part 2) A drop is visible — never silently keep showing
          // stale "live" state over a dead stream.
          setStatus("reconnecting");
        },
      });
    };

    if (canStream) openTail(tailDate);

    let tick = 0;
    const timer = setInterval(() => {
      const nd = todayUTC();
      if (nd !== tailDate) {
        handle?.close();
        tailDate = nd;
        reloadWindowForNewDay(queryClient, nd);
        if (canStream) openTail(nd);
      }
      tick += 1;
      if (tick % 4 === 0) void reconcile(queryClient, tailDate, doFetch);
    }, tickMs ?? PRESENCE_POLL_MS);

    return () => {
      cancelled = true;
      clearInterval(timer);
      handle?.close();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- eventSourceFactory/fetchImpl/tickMs are test-only seams, stable (undefined) in production.
  }, [enabled, queryClient, eventSourceFactory, fetchImpl, tickMs]);

  return status;
}
