import { useEffect } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import type { FlowRecordsResponse } from "../../types/handwritten";

/**
 * `#mission=<id>` — `viewer.html`'s `catalogQuery()` mission branch. Fetches
 * `/flow-mission/<id>` (the "replay this mission" payload, across every day
 * file the mission touched) the same way `boot()` does, then makes the same
 * call legacy's own guard does:
 *
 * ```
 * if(cq&&cq.kind==="mission"&&RAW.length){ location.href="/mission/"+id+"/graph"; return; }
 * ```
 *
 * A populated response means a real, working destination already exists
 * (`/mission/<id>/graph` — the vendored React Flow mission-graph page, a
 * SEPARATE document with its own bundle, permanently out of `/next`'s scope
 * per the scaffold's own `mission-redirect` doc) — so this is a REAL
 * navigation, not a not-ported placeholder, exactly matching legacy. Packet
 * 1 left `#mission=<id>` as an inert `LensPlaceholder`, deferred as "out of
 * scope for this packet"; this packet is the one that owns replay-by-query,
 * so it completes the wiring Packet 1 left undone rather than leaving a
 * second placeholder on top of a route that already knows exactly where to
 * go.
 *
 * An EMPTY response (HTTP 200, `count:0` — this corpus's own honest
 * `/flow-mission/:id` mock, matching what the REAL endpoint returns for an
 * unmatched id; see `tests/parity/lib/mock-routes.js`'s own comment) is a
 * genuine no-data state, not a gap — legacy's own fallback here is an empty
 * fleet render, which `/next`'s fleet lens isn't itself byte-ported yet
 * (Packet 5's territory), so this renders an honest, minimal "nothing to
 * replay" message instead of reaching for machinery this packet doesn't own.
 * A non-2xx response (a genuinely unreachable daemon — the ONLY way this
 * corpus's blank-route harness reaches this endpoint, since a real daemon
 * itself never 404s here) is the separate error branch below, which the
 * empty branch is deliberately NOT folded into (see `FleetStrip`'s own
 * precedent for reading `fetchJson`'s real status/message rather than a bare
 * boolean).
 */
export function MissionReplay({ missionId }: { missionId: string }) {
  const query = useQuery({
    queryKey: queryKeys.flowMission(missionId),
    queryFn: () => fetchJson<FlowRecordsResponse>(`/flow-mission/${encodeURIComponent(missionId)}`),
  });

  const reachable = query.data?.ok === true && query.data.data.count > 0;

  // Navigation is a side effect, not something to trigger mid-render — fires
  // once the fetch resolves reachable, mirroring boot()'s own
  // fetch-then-navigate sequencing.
  useEffect(() => {
    if (reachable) {
      location.href = `/mission/${encodeURIComponent(missionId)}/graph`;
    }
  }, [reachable, missionId]);

  if (!query.data) {
    return (
      <div data-state="pending" role="status" aria-label={`Loading mission ${missionId}`}>
        <div className="stagehdr">mission replay</div>
        <div className="none">loading…</div>
      </div>
    );
  }

  if (!query.data.ok) {
    return (
      <div data-state="error" role="alert">
        <div className="stagehdr">mission replay</div>
        <div className="none">
          couldn't reach /flow-mission/{missionId}
          {query.data.status !== null ? ` (HTTP ${query.data.status})` : ""}: {query.data.message}
        </div>
      </div>
    );
  }

  if (reachable) {
    // The effect above is already navigating away — nothing meaningful to
    // paint (legacy's own boot() `return`s immediately after setting
    // location.href, never calling render() on this path either).
    return (
      <div data-state="navigating" role="status">
        <div className="none">opening mission {missionId}…</div>
      </div>
    );
  }

  return (
    <div data-state="empty">
      <div className="stagehdr">mission replay</div>
      <div className="none">no records found for mission {missionId} — nothing to replay yet.</div>
    </div>
  );
}
