import { useQuery } from "@tanstack/react-query";
import { useSessionLiveness } from "./useSessionLiveness";
import { fetchJson } from "../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS } from "../lib/queryKeys";
import type { Route } from "../lib/route";
import { shapeRecords } from "../lib/flow";
import { getSource } from "../lib/source";
import { useDay } from "./useDay";
import type { FlowWindowResult } from "./useFlowWindow";
import { ingest, type NormRecord } from "../lib/ingest";
import { sessionRouteRecords } from "../lib/runRef";
import type { FlowRecordsResponse } from "../types/generated/FlowRecordsResponse";

/**
 * Which records does THIS route actually mean? (#1800 P1)
 *
 * Until this existed, `App.tsx` fed `EventLogColumn` the live rolling window
 * (`useFlowWindow`) on EVERY route that shows an event log. `showsEventLog()`
 * returns true for `playback` and `session` — it only excludes runs/console/
 * machine — so a `#dispatch=<id>` route rendered the event log populated with
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
  records: NormRecord[];
  /** True while a HISTORICAL slice is still loading. The live window has its
   *  own `settled`; this is only about the fetched-slice routes, so a caller
   *  can tell "empty because still loading" from "empty because empty". */
  loading: boolean;
  /** True when these records came from a historical fetch rather than the
   *  rolling live window — lets a caller label the scope honestly. */
   historical: boolean;
  /** Why the slice is empty, when it is empty BECAUSE the fetch failed.
   *
   * Without this, a dead daemon, a 500, a typo'd session id and a genuinely
   * quiet day all render as "no events yet" — byte-identical. This repo has
   * already litigated that exact class once (`queryKeys.ts`'s
   * `LAB_POLL_FAILURE_THRESHOLD`: "a raw silent-catch made a dead daemon
   * byte-identical to an idle run"). Refusing to fall back to live records is
   * only honest if the UI can say WHY it has none. */
  error: { status: number | null; message: string } | null;
}

/** Decodes BOTH wire shapes via the shared `ingest` (`lib/ingest.ts`), the
 * same boundary `useFlowWindow` uses and legacy's own decode at
 * viewer.html:3920.
 *
 * Both endpoints answer the same `FlowRecordsResponse` envelope (D5, #3035):
 *
 *   GET /flow/<date>        -> { records, ... }    (lib.rs `flow_handler`)
 *   GET /flow-dispatch/<id>  -> { records, ... }    (`catalog_records_response`)
 *
 * (#1800) Then SHAPED through `shapeRecords`, as legacy's own playback boot
 * is `DATA=flowToRenderModel(RAW)` (viewer.html:3894/3922), so this hook and
 * `PlaybackLens` hand out the SAME record set from the same cache entry. The
 * meta line's census is what made a past gap between the two visible,
 * because it is the only surface that says the number out loud.
 *
 * Shared cache slot, shared decode, shared shaping: the stage, the event log
 * and the status bar cannot disagree about what the day contained. */
function recordsOf(result: { ok: true; data: unknown } | { ok: false } | undefined): NormRecord[] | null {
  if (!result || !result.ok) return null;
  return shapeRecords(ingest(result.data));
}

export function useRouteRecords(route: Route, flowWindow: FlowWindowResult): RouteRecords {
  const date = route.kind === "playback" ? route.date : null;
  const sessionId = route.kind === "dispatch" ? route.dispatchId : null;
  // A link naming the run's mission lists that run alone, the one the
  // page's header reads (`sessionRun` with the same mission).
  const missionId = route.kind === "dispatch" ? route.missionId : null;
  // (#1801) `date` is `null` on a playback route ONLY when a static build
  // forced it (`route.ts`'s own doc) — so reading the source's flow file directly
  // here, rather than re-deriving it from `date === null`, is the "one
  // resolver" this fix keeps to (`lib/source.ts`, the one place the build type is decided).
  // (#2065) A DISPATCH route on a static build reads the same committed file
  // and slices ONE session out of it (`session_id`), instead of asking a
  // daemon that is not there for `/flow-dispatch/<id>` (a 404 on every
  // dispatch-row tap of the demo). The file already carries every session
  // its `demo-runs.json` lists; there is nothing to fetch.
  // (U4-1) …and EVERY OTHER route on a static build reads it too, not just
  // those two. Measured on the served demo: `#lens=fleet` showed "50 of 6092
  // events" at rest while `#lens=runs`/`machine`/`console` showed "0 EVENTS"
  // on the same load — and rewinding made the count appear, so the records
  // had been there all along. The asymmetry was accidental: on a static build
  // `#lens=fleet` resolves to the PLAYBACK route (`route.ts`'s static branch),
  // which took the branch below; an explicitly-named lens kept its own route
  // kind and fell through to `flowWindow.data`, which is empty by
  // construction on a daemon-less build (`useFlowWindow`'s queries are
  // `enabled: daemonBacked`). So three of the four lenses reported an empty
  // day for a day the page had fully loaded.
  //
  // One rule now, the same one `useDay` already follows: a static build has
  // ONE committed file and it is the answer on EVERY route (sliced per
  // session on a dispatch route, whole otherwise). Costs no extra request —
  // `useDay`'s static query is one shared, `staleTime: Infinity` download.
  const source = getSource();
  const flowSrc = source.kind === "static" ? source.flow : null;
  // (#2086) The loaded day comes from ONE hook now; on a static build this
  // is the committed file (any route), on a daemon playback `/flow/<date>`.
  const day = useDay(route.kind === "playback" ? route.date : null);

  // A session drill-in is historical ONLY once the session is over. While it
  // is still running the slice has to keep refetching, or the entire route
  // freezes at whatever the first fetch happened to catch: no new events, and
  // with them nothing derived — elapsed time, stage progress, turn counts.
  // Fleet kept moving throughout (it reads the polled `flowWindow`), which is
  // what made this read as "the run lens is stuck" rather than "this query
  // never refetches".
  //
  // Liveness comes from presence heartbeats (`useLiveSessionIds`) rather than
  // from scanning the slice for a terminal bookend: presence is the
  // fleet-membership source of truth, and a slice-derived guess would call a
  // session dead the moment its `dispatch.complete` landed even though the
  // reconciler had not yet closed it. Polled only on a session route — the
  // `enabled` gate is the same one #1800 P2 added so a replay never asks the
  // daemon about NOW.
  //
  // (#2011) That reasoning stands and is unchanged. What it left unhandled is
  // the OPPOSITE race: when presence drops the session the interval goes
  // `false`, and before `useSessionLiveness` existed nothing fetched again —
  // so a page whose last live poll predated `dispatch complete` froze on that
  // snapshot permanently. `shouldPoll` is `isLive` plus a bounded grace window
  // after the drop; see that hook for why one immediate fetch is not enough.
  const { isLive: sessionIsLive, shouldPoll } = useSessionLiveness(sessionId);

  const sessionQuery = useQuery({
    queryKey: queryKeys.flowSession(sessionId ?? ""),
    queryFn: () => fetchJson<FlowRecordsResponse>(`/flow-dispatch/${encodeURIComponent(sessionId ?? "")}`),
    enabled: sessionId !== null && flowSrc === null,
    refetchInterval: shouldPoll ? PRESENCE_POLL_MS : false,
  });

  if (flowSrc !== null) {
    // `fetchStaticFlowRecords` already collapses a network failure, a 404,
    // or an empty file to `[]` (matching legacy's own silent catch — see
    // that function's own doc), so there is no distinct HTTP-status error to
    // surface here the way the daemon playback branch below does: a static build
    // has no daemon to report a status FROM. Still shaped through the SAME
    // `shapeRecords` every other branch here uses (this hook's own
    // module doc explains why that matters).
    const all = day.records ?? [];
    return {
      records: sessionId !== null ? sessionRouteRecords(all.filter((r) => r.session_id === sessionId), sessionId, missionId) : all,
      loading: day.loading,
      historical: true,
      error: null,
    };
  }

  if (date !== null) {
    // A FAILED fetch yields [] rather than the live window: showing live
    // traffic under a "playback for <date>" heading is the exact confusion
    // this hook exists to remove. Empty is honest; wrong is not. `useDay`
    // fetches `/flow/<date>` on the same `queryKeys.flowDate` slot
    // `useFlowWindow` uses, so a playback of today reuses the window's day.
    return { records: day.records ?? [], loading: day.loading, historical: true, error: day.error };
  }

  if (sessionId !== null) {
    const recs = recordsOf(sessionQuery.data);
    const err = sessionQuery.data && !sessionQuery.data.ok ? sessionQuery.data : null;
    return {
      records: sessionRouteRecords(recs ?? [], sessionId, missionId),
      loading: sessionQuery.data === undefined,
      // A running session is not a historical slice, and saying so is what
      // lets the log keep its live affordances (the window label, the
      // follow-latest tail) instead of presenting a moving feed as a replay.
      historical: !sessionIsLive,
      error: err ? { status: err.status, message: err.message } : null,
    };
  }

  // (#2965) A live route's log IS the live window, so the window's failed
  // day read is its error: otherwise the log says "no events yet" off a read
  // that never happened.
  const f = flowWindow.failure;
  return { records: flowWindow.data, loading: false, historical: false, error: f ? { status: f.status, message: f.message } : null };
}
