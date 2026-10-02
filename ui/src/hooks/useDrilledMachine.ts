import { useMemo } from "react";
import { MACHINE_NOT_FOUND_LABEL, drilledUidOf, machineLabel } from "../lib/machineKey";
import { isLiveRoute, type Route } from "../lib/route";
import type { MachineSpecsResponse } from "../types/generated/MachineSpecsResponse";
import type { PresenceBeat } from "../types/generated/PresenceBeat";
import type { NormRecord } from "../lib/ingest";
import { useFleetRoster } from "./useLiveMachines";

/** The flow window facts the label needs. */
export interface DrilledWindow {
  data: NormRecord[];
  settled: boolean;
  failure: unknown;
}

/** The machine route's drilled target. The route carries a machine KEY, not
 *  the uid (#2929), so it is resolved the way `MachineLens` does: an
 *  unresolved key names no machine. (#2921 follow-up) The roster names a
 *  drilled machine nothing else does, exactly as it names that machine's
 *  fleet card; it is read once, no poller. */
export function useDrilledMachine(
  route: Route,
  flowWindow: DrilledWindow,
  liveMachines: Map<string, PresenceBeat>,
  specs: MachineSpecsResponse | null,
): { drilledKey: string | null; drilledUid: string | null; drilledName: string | null } {
  const drilledKey = route.kind === "machine" ? route.machine : null;
  const { machines: roster } = useFleetRoster(isLiveRoute(route) && drilledKey != null, false);
  const { data, settled, failure } = flowWindow;
  const drilledUid = useMemo(
    () => drilledUidOf({ data, liveMachines, specs, roster }, drilledKey),
    [drilledKey, data, liveMachines, specs, roster],
  );
  const drilledName = useMemo(() => {
    if (drilledKey == null) return null;
    // A key naming no machine says so once the window has landed, rather
    // than inventing a label no card shows; blank while it is still loading.
    // (#2965) Not while a flow read is failing: a machine known only from
    // flow records is indistinguishable from an unknown key until it reads.
    if (drilledUid == null) return settled && failure === null ? MACHINE_NOT_FOUND_LABEL : "";
    return machineLabel({ data, liveMachines, specs, roster }, drilledUid);
  }, [drilledKey, drilledUid, data, settled, failure, liveMachines, specs, roster]);
  return { drilledKey, drilledUid, drilledName };
}
