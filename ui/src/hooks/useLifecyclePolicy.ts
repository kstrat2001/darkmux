import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { policyOf, type LifecyclePolicy } from "../lib/lifecycle";
import { queryKeys } from "../lib/queryKeys";
import { runsReachable, runsSrc } from "../lib/source";
import type { RunsResponse } from "../types/handwritten";

/** The lifecycle policy the daemon judges runs by (`/runs`'s `policy`), so
 *  every surface judges a run by the same numbers the runs board does. It
 *  rides the `/runs` query every other reader shares (the fleet lens and
 *  the runs board poll it; a page with neither reads it once). Until it
 *  answers, and from a daemon that does not publish one, the defaults. */
export function useLifecyclePolicy(): LifecyclePolicy {
  const q = useQuery({
    queryKey: queryKeys.runs(),
    queryFn: () => fetchJson<RunsResponse>(runsSrc()),
    enabled: runsReachable(),
  });
  const p = q.data?.ok ? q.data.data.policy : undefined;
  // One object per policy VALUE: callers key memos on it.
  return useMemo(() => policyOf(p), [p?.stale_after_ms, p?.budget_wait_grace_ms]); // eslint-disable-line react-hooks/exhaustive-deps -- keyed on the values, not the response object
}
