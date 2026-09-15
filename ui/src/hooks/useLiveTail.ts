import { useEffect, useState } from "react";
import { useQueryClient, type QueryClient } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS, LIVE_CONTACT_TIMEOUT_MS } from "../lib/queryKeys";
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
 * Not gated by `enabled` internally — the caller passes whether the tail
 * should run. `App.tsx` passes `isLiveRoute(route)` for its OWN (fleet-wide)
 * mount, so its copy only runs on a genuinely live route (see `lib/route.ts`'s
 * own doc for why `playback`/`session`/`mission` are excluded there,
 * mirroring legacy's `wantsPlayback` gate on `startLiveTail`).
 * `MissionGraphLens.tsx` (#1868) is a SECOND caller — it mounts its own copy
 * of this exact hook, unconditionally enabled while it's mounted, precisely
 * because the App-level copy is gated OFF for its route; see that
 * component's own doc for why one mounted copy at a time is what actually
 * runs, never two.
 */
/**
 * What the header's badge is allowed to claim. `live` means the daemon has
 * been in CONTACT with this page inside `LIVE_CONTACT_TIMEOUT_MS` — not that
 * records are arriving, and (#2683) no longer merely that `EventSource` has
 * not complained. See the silence watchdog in the ticker below.
 *
 * The deliberate boundary: a stream that goes half-open while the daemon's
 * HTTP still answers keeps reporting `live`, because the reconcile backstop
 * is still pulling current records over that working transport — the page's
 * claim ("what you are looking at is current") remains true, only the push
 * path is degraded. What the watchdog removes is the case where NEITHER
 * transport is answering and the page said `live` anyway.
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
): Promise<boolean> {
  // (#2683) Returns whether the daemon ANSWERED — at least one of the two
  // days' reads came back `ok`. Deliberately not "did records arrive": an
  // idle fleet reconciles successfully with an empty body all day long, and
  // conflating the two is exactly the watchdog misfire this function's one
  // caller has to avoid. A thrown fetch or a non-2xx is no contact.
  let contacted = false;
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
    contacted = true;
    const recs = asRecordArray(res.data);
    if (!recs.length) continue;
    queryClient.setQueryData<FlowRecord[]>(tailKey, (prev) => mergeTailRecords(prev ?? [], recs, cutMs));
  }
  return contacted;
}

export function useLiveTail(enabled: boolean, deps: UseLiveTailDeps = {}): LiveTailStatus {
  const queryClient = useQueryClient();
  // (2026-09-06, live review) Optimistic "live" before the stream has ever
  // actually opened was a false claim in two shapes: a fresh mount reports
  // "live" for the split second before the first `onOpen`, AND — the worse
  // case — when `canStream` is false (no `EventSource`, no test factory)
  // `openTail` is never called at all, so `onOpen` never fires and the
  // status would stay "live" forever even though the polling backstop
  // below is not a live stream. Starting pessimistic and only flipping to
  // "live" on a real `onOpen` fixes both: a route where streaming is
  // impossible now correctly reports "reconnecting" for its whole life.
  const [status, setStatus] = useState<LiveTailStatus>("reconnecting");
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
    // (#2683) The silence watchdog's two pieces of state.
    //
    // `liveNow` mirrors `status` inside the effect so the ticker can read it
    // without a ref and without re-running this effect on every flip — every
    // `setStatus` in this hook happens here, so the two cannot diverge.
    //
    // `lastContactMs` is the last moment the DAEMON answered this page, by
    // either transport: an SSE message, an SSE (re)connect, or a reconcile
    // fetch that came back `ok`. It is NOT "the last record we received" —
    // that is the distinction between dead and idle, and getting it wrong in
    // the other direction (a watchdog that fires on a quiet fleet) would be
    // worse than the bug it fixes, since silence is the NORMAL state of a
    // machine nobody is dispatching to.
    let liveNow = false;
    let lastContactMs = Date.now();
    const markContact = () => {
      lastContactMs = Date.now();
    };
    const runReconcile = (date: string) => {
      void reconcile(queryClient, date, doFetch).then((contacted) => {
        if (!cancelled && contacted) markContact();
      });
    };

    const openTail = (date: string) => {
      handle = startFlowTail(queryClient, queryKeys.flowTail(date), date, eventSourceFactory, {
        onOpen: () => {
          if (cancelled) return;
          setStatus("live");
          liveNow = true;
          markContact();
          // (#1480 part 1) Self-heal on RECONNECT (not the initial connect)
          // — a reconnected EventSource tails from NOW, silently dropping
          // whatever was emitted during the gap. Reconcile pulls it back in.
          if (everOpened) runReconcile(date);
          everOpened = true;
        },
        onError: () => {
          if (cancelled) return;
          // (#1480 part 2) A drop is visible — never silently keep showing
          // stale "live" state over a dead stream.
          setStatus("reconnecting");
          liveNow = false;
        },
        // A message — even one that fails to parse — proves the stream is
        // carrying bytes right now.
        onMessage: () => {
          if (cancelled) return;
          markContact();
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
      if (tick % 4 === 0) runReconcile(tailDate);
      // (#2683) The silence watchdog. `EventSource` only reports a drop it
      // NOTICES: a half-open TCP connection (the host slept, the path went
      // away without an RST) delivers no `error` event at all, so `onError`
      // above never fires and the header goes on claiming `live` over a
      // connection that will never deliver another byte. Nothing else in
      // this hook could catch that — the reconcile backstop keeps the DATA
      // roughly current when it can still reach the daemon, but it never
      // touched the status.
      //
      // Only fires while we currently claim `live`: once the status is
      // already `reconnecting`, the EventSource's own retry loop owns the
      // reconnect and forcing a second one on top of it would fight it.
      if (liveNow && Date.now() - lastContactMs >= LIVE_CONTACT_TIMEOUT_MS) {
        setStatus("reconnecting");
        liveNow = false;
        // Say it and MEAN it: `reconnecting` is a false label of its own if
        // nothing is actually reconnecting, and a half-open EventSource
        // never retries on its own. Tear it down and open a fresh one — a
        // real attempt, whose `onOpen` flips the status back and whose
        // #1480 self-heal reconcile backfills whatever the gap swallowed.
        if (canStream) {
          handle?.close();
          openTail(tailDate);
        }
      }
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
