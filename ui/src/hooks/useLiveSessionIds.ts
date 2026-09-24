import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS } from "../lib/queryKeys";
import { degradedFleetSource, type DegradedFleetSource } from "../lib/fleetCoverage";
import type { FleetSessionsLiveResponse } from "../types/handwritten";

/** `useLiveSessionIds`'s return shape — the live session set PLUS whether the
 * read that produced it could actually see the fleet.
 *
 * (#2725) `coverage` is the half this hook used to THROW AWAY.
 * `/fleet/sessions/live` carries the same `meta.sources.fleet` report
 * `/fleet/machines/live` does (`fleet_sessions_live_handler` and
 * `fleet_machines_live_handler` both call `source_state::coverage_meta`), and
 * this hook read the `sessions` array and dropped the rest — so every
 * consumer got an empty `Set` for "presence says nobody is running" and for
 * "presence could not be read", with nothing able to tell them apart.
 *
 * The app-wide `FleetCoverageNotice` (#2683) does caveat the page today, so
 * nothing was silently wrong on screen. But that cover is INCIDENTAL: it
 * comes from a different query, mounted by `App`, and it speaks about
 * machines. Returning the coverage here makes it structural — a consumer that
 * asserts something from this set has the caveat in hand at the point of the
 * claim, whether or not a notice happens to be mounted above it.
 * `useSessionLiveness` is the first consumer that actually needs it, and the
 * claim it was getting wrong is in that hook's own doc. */
export interface LiveSessionsResult {
  /** `LIVE_SESSIONS` as a `Set<session_id>`. Empty in the recorded corpus (no
   * session was live at record time); `lib/flow.ts::liveSessionSet` falls
   * back to the flow-derived heuristic when this is empty, same as legacy. */
  sessions: Set<string>;
  /** The missions those sessions run under (the beat's optional
   * `mission_id`). A mission's own run-grain session never beats, so its run
   * page is live while this set names its mission. */
  missions: Set<string>;
  /** Degraded fleet coverage for THIS read, in the shared vocabulary
   * (`lib/fleetCoverage.ts`), or `null` when presence is healthy, switched
   * off, or has not answered yet. `null` is the ordinary case and says
   * nothing — a standalone machine has no fleet substrate by design, and
   * reading as degraded there would be the bug. */
  coverage: DegradedFleetSource | null;
}

/** `pollLiveSessions()` (viewer.html:3664) as a query hook. */
/** `enabled` (#1800 P2): a REPLAY must not poll live presence. Passing the
 * result away is not enough — the query still fires, still polls on
 * `refetchInterval`, and still describes NOW. This stops the request. */
export function useLiveSessionIds(enabled = true): LiveSessionsResult {
  const query = useQuery({
    enabled,
    queryKey: queryKeys.fleetSessionsLive(),
    queryFn: () => fetchJson<FleetSessionsLiveResponse>("/fleet/sessions/live"),
    refetchInterval: PRESENCE_POLL_MS,
  });

  return useMemo(() => {
    const set = new Set<string>();
    const missions = new Set<string>();
    if (query.data?.ok) {
      for (const beat of query.data.data.sessions ?? []) {
        if (beat?.session_id) set.add(beat.session_id);
        if (beat?.mission_id) missions.add(beat.mission_id);
      }
    }
    // `query.data === undefined` is pending (or disabled) — no claim either
    // way, the same rule `useFleetCoverage` states for the machines half.
    // Only a SETTLED `ok:false` is a failed read.
    const unreadable = query.data !== undefined && !query.data.ok;
    const meta = query.data?.ok ? (query.data.data.meta ?? null) : null;
    return { sessions: set, missions, coverage: degradedFleetSource(meta, unreadable) };
  }, [query.data]);
}
