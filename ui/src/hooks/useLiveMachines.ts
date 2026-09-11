import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS } from "../lib/queryKeys";
import type { CoverageMeta, FleetMachinesLiveResponse, FleetRosterResponse, PresenceBeat, RosterMachineEntry } from "../types/handwritten";
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
 * (#1855) The operator's DECLARED fleet roster — `GET /fleet/roster` —
 * independent of presence. This is the other half of "rostered-but-silent
 * machine vanishes entirely": `useLiveMachines` above can only ever report
 * a machine that is CURRENTLY beating, so a machine the operator added and
 * which is down, unreachable, or has never started its daemon was
 * previously invisible to every consumer of this file. `cards.ts`'s
 * `rosterOnlyEntries` reconciles this against the live/flow-derived uids so
 * a machine that IS already accounted for (beating, or with flow history)
 * is never double-reported.
 *
 * `enabled` (#1800 P2, same reasoning as `useLiveMachines`): a REPLAY must
 * not consult the CURRENT roster over a past day — showing today's fleet
 * membership against a recorded day is the same confidently-wrong class
 * this file's other live-only hooks already guard against.
 */
export function useFleetRoster(enabled = true): RosterMachineEntry[] {
  const query = useQuery({
    enabled,
    queryKey: queryKeys.fleetRoster(),
    queryFn: () => fetchJson<FleetRosterResponse>("/fleet/roster"),
    // Roster membership is local operator config, not a heartbeat — no need
    // for the tight presence cadence. Polling at all (rather than fetching
    // once) is what makes a `darkmux machine add` show up on an already-open
    // page without a reload, same convenience `/runs` gives the lab count.
    refetchInterval: PRESENCE_POLL_MS,
  });
  // `?? []` guards a malformed/shape-mismatched 200 the same way `runs`'s own
  // query does (`FleetLens.tsx`'s own comment on that guard) — `ok: true`
  // only proves the body parsed as JSON, not that it matches
  // `FleetRosterResponse`.
  return query.data?.ok ? (query.data.data.machines ?? []) : [];
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
