import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { queryKeys } from "../lib/queryKeys";
import type { Route } from "../lib/route";
import type { FlowRecord, FlowRecordsResponse } from "../types/handwritten";
import type { FlowWindowResult } from "./useFlowWindow";

/**
 * Which records does THIS route actually mean? (#1800 P1)
 *
 * Until this existed, `App.tsx` fed `EventLogColumn` the live rolling window
 * (`useFlowWindow`) on EVERY route that shows an event log. `showsEventLog()`
 * returns true for `playback` and `session` — it only excludes runs/console/
 * machine — so a `#session=<id>` route rendered the event log populated with
 * the LIVE window's records instead of that session's.
 *
 * That is not a missing view. It is the wrong data, displayed confidently,
 * with nothing on screen saying so: the stage said "session replay" while the
 * column beside it listed unrelated live traffic. Legacy never had this bug —
 * its `boot()` re-scopes `RAW` to the fetched slice before rendering, so the
 * log and the stage always describe the same thing.
 *
 * The fix is a routing decision, not a second pipeline: `EventLogColumn`
 * already takes `records` as a prop, so the historical slices only need
 * fetching and handing over.
 *
 * Hooks cannot be conditional, so both slice queries are always CALLED and
 * gated with `enabled` — the disabled one never fires a request and returns
 * undefined, which falls through to the live window.
 */
export interface RouteRecords {
  /** What the event log should show for this route. */
  records: FlowRecord[];
  /** True while a HISTORICAL slice is still loading. The live window has its
   *  own `settled`; this is only about the fetched-slice routes, so a caller
   *  can tell "empty because still loading" from "empty because empty". */
  loading: boolean;
  /** True when these records came from a historical fetch rather than the
   *  rolling live window — lets a caller label the scope honestly. */
  historical: boolean;
}

function recordsOf(result: { ok: true; data: FlowRecordsResponse } | { ok: false } | undefined): FlowRecord[] | null {
  if (!result || !result.ok) return null;
  return result.data.records as unknown as FlowRecord[];
}

export function useRouteRecords(route: Route, flowWindow: FlowWindowResult): RouteRecords {
  const date = route.kind === "playback" ? route.date : null;
  const sessionId = route.kind === "session" ? route.sessionId : null;

  // Deliberately the SAME cache key `useFlowWindow` uses for its two day
  // fetches (`queryKeys.flowDate`). Same endpoint, same response — sharing the
  // slot means a playback route for today reuses the window's already-fetched
  // day instead of issuing a second identical request. It also means the two
  // can never disagree about what `/flow/<date>` returned.
  const dayQuery = useQuery({
    queryKey: queryKeys.flowDate(date ?? ""),
    queryFn: () => fetchJson<FlowRecordsResponse>(`/flow/${encodeURIComponent(date ?? "")}`),
    enabled: date !== null,
  });

  const sessionQuery = useQuery({
    queryKey: queryKeys.flowSession(sessionId ?? ""),
    queryFn: () => fetchJson<FlowRecordsResponse>(`/flow-session/${encodeURIComponent(sessionId ?? "")}`),
    enabled: sessionId !== null,
  });

  if (date !== null) {
    const recs = recordsOf(dayQuery.data);
    // A FAILED fetch yields [] rather than the live window: showing live
    // traffic under a "playback for <date>" heading is the exact confusion
    // this hook exists to remove. Empty is honest; wrong is not.
    return { records: recs ?? [], loading: dayQuery.data === undefined, historical: true };
  }

  if (sessionId !== null) {
    const recs = recordsOf(sessionQuery.data);
    return { records: recs ?? [], loading: sessionQuery.data === undefined, historical: true };
  }

  return { records: flowWindow.data, loading: false, historical: false };
}
