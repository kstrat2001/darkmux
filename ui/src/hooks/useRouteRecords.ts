import { useQuery } from "@tanstack/react-query";
import { useSessionLiveness } from "./useSessionLiveness";
import { fetchJson } from "../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS } from "../lib/queryKeys";
import type { Route } from "../lib/route";
import { asRecordArray, fetchStaticFlowRecords, normalizeRecords } from "../lib/flow";
import { staticFlowSrc } from "../lib/staticSource";
import type { FlowRecord } from "../types/handwritten";
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

/** Decodes BOTH wire shapes via the shared `asRecordArray` (`lib/flow.ts:89`),
 * the same helper `useFlowWindow` uses and legacy used at viewer.html:3920.
 *
 * The two endpoints DIFFER and an earlier version of this file assumed they
 * did not — reading `.records` off both:
 *
 *   GET /flow/<date>        -> a BARE JSON ARRAY   (lib.rs `flow_handler`)
 *   GET /flow-session/<id>  -> { records, ... }    (`catalog_records_response`)
 *
 * So every playback day decoded to `undefined` -> `[]` -> a permanently empty
 * log, silently. Verified against the live daemon at the merge gate, not
 * inferred. The old signature also LIED: it was annotated `FlowRecord[] | null`
 * while returning `undefined` on the array payload, and the `?? []` at the call
 * sites was load-bearing purely by accident.
 *
 * (#1800) Then SHAPED through `normalizeRecords` — legacy's own playback boot
 * is `DATA=flowToRenderModel(RAW)` (viewer.html:3894/3922), and this hook was
 * handing out `RAW`. Two consequences, both real:
 *
 *   - The flow file's leading `{"_type":"schema"}` header stayed in the set.
 *     It has no `machine_uid`, so the meta line counted a second, phantom
 *     machine, and the event log listed it as an `Invalid Date other` row.
 *   - This hook and `PlaybackLens` produced DIFFERENT record sets from the
 *     same cache entry — the lens normalized, the log and meta line did not.
 *     One source of truth was the stated property; two sets was the fact. The
 *     meta line's census is what made the gap visible, because it is the only
 *     surface that says the number out loud.
 *
 * Shared cache slot, shared decode, shared shaping: the stage, the event log
 * and the status bar now cannot disagree about what the day contained. */
function recordsOf(result: { ok: true; data: unknown } | { ok: false } | undefined): FlowRecord[] | null {
  if (!result || !result.ok) return null;
  return normalizeRecords(asRecordArray(result.data));
}

export function useRouteRecords(route: Route, flowWindow: FlowWindowResult): RouteRecords {
  const date = route.kind === "playback" ? route.date : null;
  const sessionId = route.kind === "dispatch" ? route.dispatchId : null;
  // (#1801) `date` is `null` on a playback route ONLY when `isStaticBuild()`
  // forced it (`route.ts`'s own doc) — so reading `staticFlowSrc()` directly
  // here, rather than re-deriving it from `date === null`, is the "one
  // resolver" this fix keeps to (`lib/staticSource.ts`'s module doc).
  // (#2065) A DISPATCH route on a static build reads the same committed file
  // and slices ONE session out of it (`session_id`), instead of asking a
  // daemon that is not there for `/flow-session/<id>` (a 404 on every
  // dispatch-row tap of the demo). The file already carries every session
  // its `demo-runs.json` lists; there is nothing to fetch.
  const flowSrc = route.kind === "playback" || route.kind === "dispatch" ? staticFlowSrc() : null;

  // Deliberately the SAME cache key `useFlowWindow` uses for its two day
  // fetches (`queryKeys.flowDate`). Same endpoint, same response — sharing the
  // slot means a playback route for today reuses the window's already-fetched
  // day instead of issuing a second identical request. It also means the two
  // can never disagree about what `/flow/<date>` returned.
  const dayQuery = useQuery({
    queryKey: queryKeys.flowDate(date ?? ""),
    queryFn: () => fetchJson<unknown>(`/flow/${encodeURIComponent(date ?? "")}`),
    enabled: date !== null,
  });

  // (#1801) The static-demo twin of `dayQuery` above — same cache slot
  // `PlaybackLens` reads for the stage, so the event log and the stage can
  // never disagree about the committed file's contents (`queryKeys.ts`'s own
  // doc on `staticFlowSrc`). `date` is always `null` whenever `flowSrc` is
  // non-null (see the doc above), so this and `dayQuery` are mutually
  // exclusive by construction, not by an extra guard.
  const staticQuery = useQuery({
    queryKey: queryKeys.staticFlowSrc(flowSrc ?? ""),
    queryFn: () => fetchStaticFlowRecords(flowSrc ?? ""),
    enabled: flowSrc !== null,
  });

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
    queryFn: () => fetchJson<unknown>(`/flow-session/${encodeURIComponent(sessionId ?? "")}`),
    enabled: sessionId !== null && flowSrc === null,
    refetchInterval: shouldPoll ? PRESENCE_POLL_MS : false,
  });

  if (flowSrc !== null) {
    // `fetchStaticFlowRecords` already collapses a network failure, a 404,
    // or an empty file to `[]` (matching legacy's own silent catch — see
    // that function's own doc), so there is no distinct HTTP-status error to
    // surface here the way `dayQuery`'s branch below does: a static build
    // has no daemon to report a status FROM. Still shaped through the SAME
    // `normalizeRecords` every other branch here uses (this hook's own
    // module doc explains why that matters).
    const all = staticQuery.data ? normalizeRecords(staticQuery.data) : [];
    return {
      records: sessionId !== null ? all.filter((r) => r.session_id === sessionId) : all,
      loading: staticQuery.data === undefined,
      historical: true,
      error: null,
    };
  }

  if (date !== null) {
    const recs = recordsOf(dayQuery.data);
    const err = dayQuery.data && !dayQuery.data.ok ? dayQuery.data : null;
    // A FAILED fetch yields [] rather than the live window: showing live
    // traffic under a "playback for <date>" heading is the exact confusion
    // this hook exists to remove. Empty is honest; wrong is not.
    return {
      records: recs ?? [],
      loading: dayQuery.data === undefined,
      historical: true,
      error: err ? { status: err.status, message: err.message } : null,
    };
  }

  if (sessionId !== null) {
    const recs = recordsOf(sessionQuery.data);
    const err = sessionQuery.data && !sessionQuery.data.ok ? sessionQuery.data : null;
    return {
      records: recs ?? [],
      loading: sessionQuery.data === undefined,
      // A running session is not a historical slice, and saying so is what
      // lets the log keep its live affordances (the window label, the
      // follow-latest tail) instead of presenting a moving feed as a replay.
      historical: !sessionIsLive,
      error: err ? { status: err.status, message: err.message } : null,
    };
  }

  return { records: flowWindow.data, loading: false, historical: false, error: null };
}
