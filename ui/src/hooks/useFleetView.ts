import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS } from "../lib/queryKeys";
import { fleetViewSrc, getSource } from "../lib/source";
import type { FleetMachine } from "../types/generated/FleetMachine";
import type { FleetView } from "../types/generated/FleetView";

export interface FleetViewResult {
  /** Every machine the daemon's view holds, or `null` when the view could
   * not be read (a replay, a failed read, or nothing asked yet). */
  rows: readonly FleetMachine[] | null;
  /** The view's read has finished, successfully or not. A replay never asks,
   * so it is answered from the start. */
  answered: boolean;
}

/**
 * `GET /fleet/view`: the machine list, each machine's card outcome, its
 * liveness and what it lets this machine do, gathered once by the daemon.
 * The fleet lens's cards read these rows; nothing on the page recomputes
 * them from presence or a second probe.
 *
 * `enabled` is false for a replay: a view describes now, never a past day.
 * A static build reads its committed snapshot of the same document instead
 * (`source.ts::fleetViewSrc`), so the same rows drive the demo.
 */
export function useFleetView(enabled: boolean): FleetViewResult {
  const src = fleetViewSrc();
  const active = enabled && src !== null;
  const isDaemon = getSource().kind === "daemon";
  const query = useQuery({
    enabled: active,
    queryKey: queryKeys.fleetView(src ?? ""),
    queryFn: () => fetchJson<FleetView>(src ?? ""),
    // The daemon caches its own gather for a few seconds; a committed file
    // never changes under the page.
    refetchInterval: isDaemon ? PRESENCE_POLL_MS : false,
    staleTime: isDaemon ? 0 : Infinity,
  });
  return useMemo(() => {
    // `?? null`: `ok: true` only proves the body parsed as JSON, not that it
    // matches `FleetView`.
    const rows = query.data?.ok ? (query.data.data.machines ?? null) : null;
    return { rows, answered: !active || query.status !== "pending" };
  }, [active, query.data, query.status]);
}
