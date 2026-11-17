import { useState } from "react";
import type { FetchResult } from "../lib/fetcher";

type Failure = { status: number | null; message: string };

/** A polled or re-read query's last GOOD answer, and the failure of the
 *  latest read when it failed. A failed refetch never erases what was known:
 *  `good` stays the last answer that arrived while `failure` says the newest
 *  read did not, so a view shows that answer marked stale instead of
 *  replacing it with an error or an empty state. `good` is null only when no
 *  read has ever succeeded. The generic form of the mission header's
 *  `useKeptMissionRow` (React's "store information from previous renders"
 *  pattern). */
export function useKeptRead<T>(data: FetchResult<T> | undefined): { good: T | null; failure: Failure | null } {
  const [kept, setKept] = useState<T | null>(null);
  const fresh = data?.ok ? data.data : null;
  if (fresh !== null && fresh !== kept) setKept(fresh);
  const failure = data && !data.ok ? data : null;
  return { good: fresh ?? kept, failure };
}
