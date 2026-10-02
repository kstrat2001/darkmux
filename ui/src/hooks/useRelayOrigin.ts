import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { queryKeys } from "../lib/queryKeys";
import { runsReachable, runsSrc } from "../lib/source";
import type { RunRelay } from "../types/generated/RunRelay";
import type { RunsResponse } from "../types/generated/RunsResponse";

/** (#3016) Where the run a session page shows was asked, when it was relayed
 * work: the machine that asked, read from the run's own row. The daemon owns
 * the session-id grammar (`SessionId::parse`), so the viewer never re-parses
 * a session id; it reads the typed `relay` the runs board already carries,
 * through the board's own query and cache, so there is no second poll and no
 * refetch. `null` for work
 * that ran where it was asked, or when the row is not in the window. */
export function useRelayOrigin(sessionId: string): RunRelay | null {
  const q = useQuery({
    queryKey: queryKeys.runs(),
    queryFn: () => fetchJson<RunsResponse>(runsSrc()),
    enabled: runsReachable(),
    // Reads the runs board's own cached answer; it only fetches when nothing
    // has (a deep link straight to a session), and never refetches on its own.
    staleTime: Infinity,
    refetchOnMount: false,
    refetchOnWindowFocus: false,
  });
  // `?? []`: a malformed 200 (a test double, API drift) is no row, not a throw.
  if (!q.data?.ok) return null;
  const row = (q.data.data.runs ?? []).find((r) => r.dispatch_id === sessionId || r.id === sessionId);
  return row?.relay ?? null;
}
