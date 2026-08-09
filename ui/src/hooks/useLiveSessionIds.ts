import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS } from "../lib/queryKeys";
import type { FleetSessionsLiveResponse } from "../types/handwritten";

/** `pollLiveSessions()` (viewer.html:3664) as a query hook — `LIVE_SESSIONS`
 * as a `Set<session_id>`. Empty in the recorded corpus (no session was live
 * at record time); `lib/flow.ts::liveSessionSet` falls back to the
 * flow-derived heuristic when this is empty, same as legacy. */
export function useLiveSessionIds(): Set<string> {
  const query = useQuery({
    queryKey: queryKeys.fleetSessionsLive(),
    queryFn: () => fetchJson<FleetSessionsLiveResponse>("/fleet/sessions/live"),
    refetchInterval: PRESENCE_POLL_MS,
  });

  return useMemo(() => {
    const set = new Set<string>();
    if (query.data?.ok) {
      for (const beat of query.data.data.sessions ?? []) {
        if (beat?.session_id) set.add(beat.session_id);
      }
    }
    return set;
  }, [query.data]);
}
