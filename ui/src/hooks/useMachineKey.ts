import { useEffect, useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { queryKeys } from "../lib/queryKeys";
import { MACHINE_NOT_FOUND_KEY, UID_SHAPED, decodeMachineKey, type DecodedMachineKey, type MachineKeyContext } from "../lib/machineKey";
import type { FleetMachinesLiveResponse } from "../types/generated/FleetMachinesLiveResponse";
import type { FleetRosterResponse } from "../types/generated/FleetRosterResponse";
import type { MachineSpecsResponse } from "../types/generated/MachineSpecsResponse";
import { useFleetRoster, useLiveMachines } from "./useLiveMachines";
import type { NormRecord } from "../lib/ingest";

/** (#2929) What a page needs to resolve a hash machine key: the same four
 * inputs the fleet card that minted the key was labeled from — presence,
 * this daemon's specs, the roster (read once, no poller of its own) — plus
 * the caller's records, and whether every one of them has SETTLED.
 *
 * `settled` matters because the answer can change as inputs land: a beat's
 * `display_name` or a roster entry can name a machine that was unnamed
 * a moment earlier. So an old uid link is
 * rewritten to its key only once everything the key depends on is in. */
export function useMachineKeyContext(
  data: NormRecord[],
  dataSettled: boolean,
  /** Live, daemon-backed page: read presence, specs and roster. `false` on a
   *  static build or a replay, where the key was minted without them. */
  live: boolean,
): { ctx: MachineKeyContext; settled: boolean } {
  const liveMachines = useLiveMachines(live);
  const { machines: roster } = useFleetRoster(live, false);
  const specsQuery = useQuery({
    enabled: live,
    queryKey: queryKeys.machineSpecs(),
    queryFn: () => fetchJson<MachineSpecsResponse>("/machine/specs"),
  });
  // Observers of the two shared slots above, for their settle state only
  // (`useLiveMachines`/`useFleetRoster` return data, not status). Disabled:
  // the hooks above own the fetch.
  const presenceState = useQuery({
    enabled: false,
    queryKey: queryKeys.fleetMachinesLive(),
    queryFn: () => fetchJson<FleetMachinesLiveResponse>("/fleet/machines/live"),
  });
  const rosterState = useQuery({
    enabled: false,
    queryKey: queryKeys.fleetRoster(),
    queryFn: () => fetchJson<FleetRosterResponse>("/fleet/roster"),
  });
  const specsData = live && specsQuery.data?.ok ? specsQuery.data.data : null;
  const ctx = useMemo(
    () => ({ data, liveMachines, specs: specsData, roster }),
    [data, liveMachines, specsData, roster],
  );
  const settled =
    dataSettled &&
    (!live || (specsQuery.status !== "pending" && presenceState.status !== "pending" && rosterState.status !== "pending"));
  return { ctx, settled };
}

/** Resolve `key` (the hash's machine value) against `ctx`, and — once the
 * context has settled — rewrite an old uid-carrying link to the key via
 * `rewrite`, which the caller builds from its own route (so the rewrite is
 * `replaceState`, no history entry, like every canonical-hash write). */
export function useDecodedMachineKey(
  key: string | null,
  ctx: MachineKeyContext,
  settled: boolean,
  rewrite: (canonicalKey: string) => void,
): DecodedMachineKey | null {
  const decoded = useMemo(() => (key == null ? null : decodeMachineKey(ctx, key)), [key, ctx]);
  // An old uid link or an outgrown key rewrites to the machine's current key;
  // (C4) a uid-shaped value that still names nothing once everything has
  // landed rewrites to the not-found marker, so the uid does not sit in the
  // address bar for as long as the tab stays open.
  const rewriteTo = !settled || decoded == null
    ? null
    : decoded.stale && decoded.key
      ? decoded.key
      : decoded.uid == null && key != null && UID_SHAPED.test(key)
        ? MACHINE_NOT_FOUND_KEY
        : null;
  useEffect(() => {
    if (!rewriteTo) return undefined;
    // After the commit, not inside it. A lens mounts in the SAME commit as the
    // route change that brought it (an old link opened, or pasted over the
    // current page), and child effects run before the app root's
    // `useSyncHash`, which then writes the route it rendered with — the old
    // uid — straight back over this rewrite. Measured: the rewrite fired and
    // the hash still read the uid. A zero-delay timer lands after it.
    const id = setTimeout(() => rewrite(rewriteTo), 0);
    return () => clearTimeout(id);
    // `rewrite` is rebuilt every render by its caller; the key is what matters.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rewriteTo]);
  return decoded;
}
