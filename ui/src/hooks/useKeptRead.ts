import { hashKey, useQueryClient, type QueryClient, type QueryKey } from "@tanstack/react-query";
import type { FetchResult } from "../lib/fetcher";

type Failure = { status: number | null; message: string };

/** Each QueryClient's last GOOD answer per query key. `fetchJson` never
 *  throws, so a failed read is the query's data and the query cache itself
 *  holds no earlier success to fall back on; this does. Keyed by client so it
 *  lives exactly as long as the client's cache (one per app, one per test). */
const lastGood = new WeakMap<QueryClient, Map<string, unknown>>();

function storeOf(client: QueryClient): Map<string, unknown> {
  let store = lastGood.get(client);
  if (!store) {
    store = new Map();
    lastGood.set(client, store);
  }
  return store;
}

/** A polled or re-read query's last GOOD answer, and the failure of the
 *  latest read when it failed. A failed refetch never erases what was known:
 *  `good` stays the last answer that arrived while `failure` says the newest
 *  read did not, so a view shows that answer marked stale instead of
 *  replacing it with an error or an empty state. `good` is null only when no
 *  read of `key` has ever succeeded on this QueryClient; it survives the view
 *  unmounting and mounting again. The generic form of the mission header's
 *  `useKeptMissionRow`. Recording during render is idempotent: it only ever
 *  stores the answer this render is showing. */
export function useKeptRead<T>(key: QueryKey, data: FetchResult<T> | undefined): { good: T | null; failure: Failure | null } {
  const store = storeOf(useQueryClient());
  const hash = hashKey(key);
  if (data?.ok) store.set(hash, data.data);
  const failure = data && !data.ok ? data : null;
  return { good: (store.get(hash) as T | undefined) ?? null, failure };
}
