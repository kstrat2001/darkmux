import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { recordsAsOf, stepIdOf, type NormRecord } from "../lib/ingest";
import { localMachineUid, machinePageRecords } from "../lib/machineIdentity";
import { queryKeys } from "../lib/queryKeys";
import type { Route } from "../lib/route";
import type { Source } from "../lib/source";
import type { MachineSpecsResponse } from "../types/generated/MachineSpecsResponse";
import type { PresenceBeat } from "../types/generated/PresenceBeat";

const NO_RECORDS: NormRecord[] = [];
/** A static build has no presence beats. */
const NO_LIVE_MACHINES: Map<string, PresenceBeat> = new Map();

/** What the event log column needs to know about the page. */
export interface EventLogInputs {
  route: Route;
  source: Source;
  /** The route's own slice of records (a playback or dispatch day, the live window). */
  routeRecords: NormRecord[];
  /** The loaded day a static build scrubs; `null` while it downloads. */
  dayRecords: NormRecord[] | null;
  missionRecords: NormRecord[] | null;
  playhead: number | null;
  /** The machine key the machine route drills into; `null` for this machine. */
  drilledKey: string | null;
  /** What that key resolved to; `null` while unresolved. */
  drilledUid: string | null;
  /** This daemon's own uid, once `/machine/specs` has answered. */
  localUid: string | null;
}

/** This machine's uid on a static build, which has no daemon specs: the
 *  machine fixture's own machine, found in the committed day (the same
 *  fixture and query key `MachineLens` reads, so no second request). */
function useStaticSelfUid(
  { route, source, dayRecords }: Pick<EventLogInputs, "route" | "source" | "dayRecords">,
  localUid: string | null,
): string | null {
  const src = source.machine;
  const query = useQuery({
    queryKey: queryKeys.staticMachine(src ?? ""),
    queryFn: () => fetchJson<{ specs: MachineSpecsResponse }>(src as string),
    enabled: source.kind === "static" && src !== null && route.kind === "machine",
    staleTime: Infinity,
  });
  const specs = query.data?.ok ? query.data.data.specs : null;
  return useMemo(
    () => localUid ?? localMachineUid(dayRecords ?? NO_RECORDS, NO_LIVE_MACHINES, specs?.machine_id ?? null, specs?.machine_uid ?? null),
    [localUid, dayRecords, specs],
  );
}

/** Which records the event log column lists, per route. */
export function useEventLogRecords(inputs: EventLogInputs): NormRecord[] {
  const { route, source, routeRecords, dayRecords, missionRecords, playhead, drilledKey, drilledUid, localUid } = inputs;
  const pageSelfUid = useStaticSelfUid(inputs, localUid);
  return useMemo(() => {
    // Mission has no playhead concept: its fold is always the full,
    // historical record set, never scoped to a scrubbed time. (#2189) A
    // selected step scopes it by `step_id` equality, filtered here, never
    // re-fetched.
    if (route.kind === "mission") {
      const all = missionRecords ?? NO_RECORDS;
      return route.stepId ? all.filter((r) => stepIdOf(r) === route.stepId) : all;
    }
    // A static build's runs/machine/console routes have no slice of their own
    // (the live window is empty there); the day's log, scoped to the
    // playhead, is what the transport scrubs. Playback and dispatch routes
    // keep their own slice, scoped the same way.
    const own = route.kind === "playback" || route.kind === "dispatch" || source.kind === "daemon";
    const base = own ? routeRecords : (dayRecords ?? NO_RECORDS);
    // (5.0 R2) A machine page lists that machine's records, not the fleet's,
    // scoped to the playhead. An unresolved machine has none to list.
    if (route.kind === "machine") return machinePageRecords(base, drilledKey, drilledUid, pageSelfUid, playhead);
    if (playhead === null) return routeRecords;
    return recordsAsOf(base, playhead);
  }, [route, source.kind, routeRecords, dayRecords, missionRecords, playhead, drilledKey, drilledUid, pageSelfUid]);
}
