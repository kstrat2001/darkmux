import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import { LensPlaceholder } from "../../components/LensPlaceholder";
import type { FlowRecordsResponse } from "../../types/handwritten";

/**
 * `#session=<id>` — `viewer.html`'s `catalogQuery()` session branch. Fetches
 * `/flow-session/<id>` the same way `boot()` does (the exact data
 * `drillSession()` re-scopes into legacy's `renderSubsystem()` — a whole
 * separate render surface: event log, resizable detail pane, per-record
 * drill — genuinely out of THIS packet's scope; see `tests/parity/README.md`'s
 * lens inventory, which already goldens legacy's own render at
 * `goldens/session-task-list.txt`).
 *
 * Unlike `MissionReplay`, there is no existing `/next`-external destination
 * to redirect to that's cheaper than building the real thing — the session
 * drill-in view doesn't exist as a standalone page the way the mission graph
 * does, it only exists AS legacy's own in-page render. So a populated fetch
 * here is a genuine "not ported yet" gap (packet 3's `NOT_PORTED_NOTICE`
 * precedent, reused via `LensPlaceholder` rather than re-inventing the
 * wording), not a redirect target — this component still does the REAL
 * fetch (closing the "replay navigation wiring" half of this packet's brief)
 * even though the render itself waits on a future session/subsystem lens
 * packet.
 *
 * An EMPTY response is a genuine no-data state (this corpus's own
 * `flow-session-task-list.json` fixture is non-empty, so the parity spec
 * exercises the populated branch — the empty branch is honest but
 * unexercised by this corpus, same shape as `MissionReplay`'s empty case).
 */
export function SessionReplay({ sessionId }: { sessionId: string }) {
  const query = useQuery({
    queryKey: queryKeys.flowSession(sessionId),
    queryFn: () => fetchJson<FlowRecordsResponse>(`/flow-session/${encodeURIComponent(sessionId)}`),
  });

  if (!query.data) {
    return (
      <div data-state="pending" role="status" aria-label={`Loading session ${sessionId}`}>
        <div className="stagehdr">session replay</div>
        <div className="none">loading…</div>
      </div>
    );
  }

  if (!query.data.ok) {
    return (
      <div data-state="error" role="alert">
        <div className="stagehdr">session replay</div>
        <div className="none">
          couldn't reach /flow-session/{sessionId}
          {query.data.status !== null ? ` (HTTP ${query.data.status})` : ""}: {query.data.message}
        </div>
      </div>
    );
  }

  const count = query.data.data.count;

  if (count === 0) {
    return (
      <div data-state="empty">
        <div className="stagehdr">session replay</div>
        <div className="none">no records found for session {sessionId}.</div>
      </div>
    );
  }

  return (
    <LensPlaceholder
      label={`session drill-in ${sessionId} — ${count} record${count === 1 ? "" : "s"} loaded, not yet rendered in /next`}
    />
  );
}
