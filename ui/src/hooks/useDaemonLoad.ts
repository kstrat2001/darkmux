/**
 * (#2107, #1833) The daemon-side continuous host sampler's live reading —
 * `/machine/resources`'s `load` block — polled independently of any
 * dispatch, so the machine drawer/modal have a `now` reading (and, off a
 * mission/dispatch route, `avg`/`max` too) once OPENED, not only while a
 * dispatch happens to be running. Before this hook, the ONLY host samples
 * anywhere in the viewer were per-dispatch `telemetry.process` flow
 * records, so the drawer read "idle · no samples in the last 10 min"
 * between dispatches (`components/machineStatsContent.tsx`'s own doc).
 *
 * **Polling is gated on `enabled` (the operator's warm-up finding, #2107 /
 * #1833 second round): closed → zero fetches scheduled, open → polls at
 * `DAEMON_LOAD_POLL_MS`, close → stops immediately.** The pill/tab reads a
 * static "Machine info" label at rest (`machineStatsContent.tsx`'s doc) —
 * there is no closed-state numeral to keep warm, so polling while closed
 * would cost a real network round trip for a number nobody can see. The
 * caller (`machineStatsContent.tsx`) passes whether ITS surface (the
 * desktop dialog, or the phone drawer's Machine tab) is currently open.
 *
 * Poll cadence while enabled: 3s, and only while the tab is
 * visible/focused. No custom `visibilitychange` wiring — TanStack Query's
 * default (`refetchIntervalInBackground: false`) already pauses the
 * interval once the document loses focus/visibility, which is the
 * library's built-in behavior, not something this hook re-implements.
 *
 * **The viewer never computes avg/max/p95 itself across polls.** The
 * daemon's ring samples continuously regardless of whether any viewer is
 * watching, so the very FIRST poll after opening already carries the full
 * `load.window` reduction (`mean_pct`/`p95_pct`/`max_pct`/`samples`/
 * `span_ms`) for however much history the ring actually holds — this hook
 * (and `effectiveHostAggregate` downstream) hands that block through
 * verbatim, never accumulating a client-side rolling window across
 * multiple polls. If the daemon's ring is younger than its 10-minute
 * ceiling (a daemon that just started), `span_ms`/`samples` say so
 * honestly and `machineStatsContent.tsx`'s `daemonWindowLabel` reflects the
 * ACTUAL span rather than always claiming "last 10 min".
 *
 * Gated on `getSource().kind` exactly like `MachineLens.tsx`'s own
 * `resourcesQuery` (#2019's "settled two ways" rule) — a static build has
 * no daemon to poll, so this reads the committed fixture's
 * `resources.load` once instead (`staleTime: Infinity`, matching that
 * lens's own `staticMachineQuery`). Shares BOTH query keys with
 * `MachineLens.tsx` on purpose: two observers on the same key share one
 * cache entry, so mounting this hook alongside that lens costs no extra
 * network round trip.
 */
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { queryKeys } from "../lib/queryKeys";
import { getSource } from "../lib/source";
import type {
  MachineLoad,
  MachineResources,
  MachineSpecs,
} from "../types/handwritten";

export const DAEMON_LOAD_POLL_MS = 3_000;

/** `enabled` — whether the caller's own surface is currently OPEN (visible
 * to the operator). `false` disables BOTH the live and static queries
 * entirely: no fetch is scheduled, and any in-flight polling interval
 * stops. See this module's own doc for why closed means zero network
 * cost, not merely a slower cadence. */
export function useDaemonLoad(enabled: boolean): MachineLoad | null {
  const source = getSource();
  const daemonBacked = source.kind === "daemon";
  const machineSrc = source.machine;

  const liveQuery = useQuery({
    queryKey: queryKeys.machineResources(),
    queryFn: () => fetchJson<MachineResources>("/machine/resources"),
    refetchInterval: DAEMON_LOAD_POLL_MS,
    enabled: daemonBacked && enabled,
  });

  const staticQuery = useQuery({
    queryKey: queryKeys.staticMachine(machineSrc ?? ""),
    queryFn: () =>
      fetchJson<{ specs: MachineSpecs; resources: MachineResources }>(
        machineSrc as string,
      ),
    enabled: !daemonBacked && machineSrc !== null && enabled,
    staleTime: Infinity,
  });

  if (daemonBacked) {
    return liveQuery.data?.ok ? (liveQuery.data.data.load ?? null) : null;
  }
  return staticQuery.data?.ok
    ? (staticQuery.data.data.resources.load ?? null)
    : null;
}
