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
 * not in the window. */
export function useRunRow(sessionId: string, poll = false): Run | null {
  const q = useQuery({
    queryKey: queryKeys.runs(),
    queryFn: () => fetchJson<RunsResponse>(runsSrc()),
    enabled: runsReachable(),
    // Reads the runs board's own cached answer; it only fetches when nothing
    // has (a deep link straight to a session).
    ...(poll ? { staleTime: PRESENCE_POLL_MS, refetchInterval: PRESENCE_POLL_MS } : { staleTime: Infinity, refetchOnMount: false, refetchOnWindowFocus: false }),
  });
  // `?? []`: a malformed 200 (a test double, API drift) is no row, not a throw.
  if (!q.data?.ok) return null;
  return (q.data.data.runs ?? []).find((r) => r.dispatch_id === sessionId || r.id === sessionId) ?? null;
}
