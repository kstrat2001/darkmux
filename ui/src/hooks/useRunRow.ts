import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { PRESENCE_POLL_MS, queryKeys } from "../lib/queryKeys";
import { runsReachable, runsSrc } from "../lib/source";
import type { Run } from "../types/generated/Run";
import type { RunsResponse } from "../types/generated/RunsResponse";

/** The run row a session page shows, from the runs board's own query and cache
 * (#3016): the daemon owns the session-id grammar (`SessionId::parse`) and the
 * facts decided once on the row (`relay`, `not_reporting`), so the page reads
 * them rather than deriving its own. Without `poll` it never refetches on its
 * own (a deep link straight to a session fetches once); a page at the live edge
 * polls so the row's `not_reporting` follows the fleet. `null` when the row is
 * not in the window. A mission row carries only its earliest session as
 * `dispatch_id`, so a later step's page finds it by `missionId`. */
export function useRunRow(sessionId: string, missionId: string | null, poll = false): Run | null {
  return useRunRows(poll).find((r) => r.dispatch_id === sessionId || r.id === sessionId || (missionId !== null && r.id === missionId)) ?? null;
}

const NO_ROWS: readonly Run[] = [];

/** The runs board's rows, from its own query and cache (see `useRunRow`);
 *  empty when `/runs` has not answered. */
export function useRunRows(poll = false): readonly Run[] {
  const q = useQuery({
    queryKey: queryKeys.runs(),
    queryFn: () => fetchJson<RunsResponse>(runsSrc()),
    enabled: runsReachable(),
    // Reads the runs board's own cached answer; it only fetches when nothing
    // has (a deep link straight to a session).
    ...(poll ? { staleTime: PRESENCE_POLL_MS, refetchInterval: PRESENCE_POLL_MS } : { staleTime: Infinity, refetchOnMount: false, refetchOnWindowFocus: false }),
  });
  // `?? NO_ROWS`: a malformed 200 (a test double, API drift) is no row, not a throw.
  if (!q.data?.ok) return NO_ROWS;
  return q.data.data.runs ?? NO_ROWS;
}
