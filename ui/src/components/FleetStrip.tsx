import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS } from "../lib/queryKeys";
import type { PresenceBeat } from "../types/handwritten";

/**
 * The scaffold's ONE real region (per the packet brief): a live strip over
 * `GET /fleet/machines/live`, driven by `useQuery`. All three data states
 * are VISIBLY DISTINCT — this is the empty-is-never-silent contract's first
 * enforcement point (the three-state arc proper lands after the full port;
 * this is the proof the pattern works, not the full contract).
 *
 * `fetchJson` never throws (see its own doc) — the pending/error/data split
 * below reads `query.data.ok`, not `query.isError`, so a non-2xx or a
 * network failure renders the SAME explicit "couldn't reach the daemon"
 * state a thrown error would, but with the actual status/message attached.
 */
export function FleetStrip() {
  const query = useQuery({
    queryKey: queryKeys.fleetMachinesLive(),
    queryFn: () => fetchJson<PresenceBeat[]>("/fleet/machines/live"),
    refetchInterval: PRESENCE_POLL_MS,
  });

  // Guard on `!query.data` (not `query.isPending`) so TypeScript actually
  // narrows `query.data` to defined for every branch below — `isPending`
  // alone doesn't carry that relationship for the type checker.
  if (!query.data) {
    return (
      <div className="fleet-strip fleet-strip--pending" data-state="pending" role="status" aria-label="Loading fleet">
        <div className="fleet-skeleton" />
        <div className="fleet-skeleton" />
      </div>
    );
  }

  if (!query.data.ok) {
    return (
      <div className="fleet-strip fleet-strip--error" data-state="error" role="alert">
        <span className="fleet-strip__icon">!</span>
        <span>
          Couldn't reach the fleet-presence endpoint
          {query.data.status !== null ? ` (HTTP ${query.data.status})` : ""}: {query.data.message}
        </span>
      </div>
    );
  }

  const machines = query.data.data;

  if (machines.length === 0) {
    return (
      <div className="fleet-strip fleet-strip--empty" data-state="empty">
        No machines currently present. A machine shows up here once its
        darkmux daemon is running and heartbeating (needs
        <code> DARKMUX_REDIS_URL</code> configured fleet-wide).
      </div>
    );
  }

  return (
    <div className="fleet-strip fleet-strip--data" data-state="data">
      {machines.map((m: PresenceBeat) => (
        <div className="fleet-card" key={m.machine_uid}>
          <div className="fleet-card__name">{m.display_name}</div>
          {m.specs && <div className="fleet-card__specs">{m.specs}</div>}
          {m.loaded_models && m.loaded_models.length > 0 && (
            <div className="fleet-card__models">{m.loaded_models.join(", ")}</div>
          )}
        </div>
      ))}
    </div>
  );
}
