import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS } from "../lib/queryKeys";
import type { CoverageMeta, FleetMachinesLiveResponse, PresenceBeat } from "../types/handwritten";
import { getSource } from "../lib/source";

/** (#2067) The committed fleet snapshot a daemon-less build ships
 * (`darkmux-fleet-src`), as the same uid-keyed map `useLiveMachines`
 * returns — so `cards.ts::specOf` can read a hardware line from it without
 * knowing which build it is on. Fetched once (no poll: a file does not
 * change under the page) and only when the meta names one; every other
 * build gets an empty map. Feed this to the SPEC lookup only, never to
 * presence: a snapshot says what the hardware is, not who is online now. */
export function useStaticFleetBeats(): Map<string, PresenceBeat> {
  const source = getSource();
  const src = source.fleet;
  const query = useQuery({
    enabled: src !== null,
    queryKey: queryKeys.staticFleet(src ?? ""),
    queryFn: () => fetchJson<FleetMachinesLiveResponse>(src ?? ""),
    // A committed file does not change under the page: never stale, so a
    // remount or tab focus does not re-download it (matches the other
    // static twins, `MachineLens`/`MissionGraphLens`).
    staleTime: Infinity,
  });
  return useMemo(() => {
    const map = new Map<string, PresenceBeat>();
    if (query.data?.ok) {
      for (const beat of query.data.data.machines ?? []) {
        if (beat?.machine_uid) map.set(beat.machine_uid, beat);
      }
    }
    return map;
  }, [query.data]);
}

/** `pollLiveMachines()` (viewer.html:3678) as a query hook — `LIVE_MACHINES`
 * as a `Map<machine_uid, PresenceBeat>`, same key shape legacy builds. */
/** `enabled` (#1800 P2): a REPLAY must not poll live presence. Passing the
 * result away is not enough — the query still fires, still polls on
 * `refetchInterval`, and still describes NOW. This stops the request. */
export function useLiveMachines(enabled = true): Map<string, PresenceBeat> {
  const query = useQuery({
    enabled,
    queryKey: queryKeys.fleetMachinesLive(),
    queryFn: () => fetchJson<FleetMachinesLiveResponse>("/fleet/machines/live"),
    refetchInterval: PRESENCE_POLL_MS,
  });

  return useMemo(() => {
    const map = new Map<string, PresenceBeat>();
    if (query.data?.ok) {
      for (const beat of query.data.data.machines ?? []) {
        if (beat?.machine_uid) map.set(beat.machine_uid, beat);
      }
    }
    return map;
  }, [query.data]);
}

/**
 * The COVERAGE half of the same presence query (#1729) — whether the fleet
 * substrate could actually be read, as opposed to being genuinely quiet.
 *
 * Split out rather than folded into `useLiveMachines` so existing callers
 * that only want the beats are untouched. It shares the query key, so this
 * costs no extra request: TanStack serves both from one cache entry.
 *
 * Returns `null` while pending or on a transport failure — the caller's own
 * pending/error handling owns those, and inventing a state here would be the
 * fabrication this contract exists to prevent.
 */
/** `enabled` (#1800 P2) — same reason as `useLiveMachines`: on a replay this
 * banner would report the CURRENT fleet's coverage over a past day's records. */
export function useFleetCoverage(enabled = true): CoverageMeta | null {
  const query = useQuery({
    enabled,
    queryKey: queryKeys.fleetMachinesLive(),
    queryFn: () => fetchJson<FleetMachinesLiveResponse>("/fleet/machines/live"),
    refetchInterval: PRESENCE_POLL_MS,
  });
  return query.data?.ok ? (query.data.data.meta ?? null) : null;
}
