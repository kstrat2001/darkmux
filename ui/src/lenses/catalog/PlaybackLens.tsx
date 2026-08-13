import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import { asRecordArray, computeTMax, computeTMin, normalizeRecords } from "../../lib/flow";
import { FleetLens } from "../fleet/FleetLens";


/**
 * The `playback` route — a bare `#<date>` hash.
 *
 * (#1800 P2) `playback-date.txt` shows what this actually is: **the fleet hero
 * rendered over one historical day**, not a separate view. Same stage
 * (TOKENS / LOCAL / CLOUD, the machine cards, the activity timeline), different
 * records. So this composes `FleetLens` rather than reimplementing it — the
 * port's earlier note about needing "a SECOND, parallel fleet-rendering
 * pipeline" was true when written, before Packet 5 built the first one.
 *
 * `historical` is passed so the hero drops `/fleet/machines/live` and
 * `/fleet/sessions/live`. Those endpoints describe NOW; asserting today's
 * presence over a replayed day is the "confidently wrong" failure `FleetLens`'s
 * own doc warns about. A replay knows what its records know.
 *
 * The day fetch shares `queryKeys.flowDate` with `useFlowWindow` and
 * `useRouteRecords` deliberately — one cache slot for one endpoint, so the
 * stage and the event log beside it can never disagree about the day.
 */
export function PlaybackLens({ date }: { date: string }) {
  const query = useQuery({
    queryKey: queryKeys.flowDate(date),
    queryFn: () => fetchJson<unknown>(`/flow/${encodeURIComponent(date)}`),
  });

  if (!query.data) {
    return (
      <div data-state="pending" role="status" aria-label={`Loading ${date}`}>
        <div className="stagehdr">playback</div>
        <div className="none">loading…</div>
      </div>
    );
  }

  if (!query.data.ok) {
    return (
      <div data-state="error" role="alert">
        <div className="stagehdr">playback</div>
        <div className="none">
          couldn't reach /flow/{date}
          {query.data.status !== null ? ` (HTTP ${query.data.status})` : ""}: {query.data.message}
        </div>
      </div>
    );
  }

  // `/flow/<date>` returns a BARE ARRAY, not `{records}` — decoded through the
  // shared `asRecordArray` like `useFlowWindow` and legacy both do. An earlier
  // draft read `.records` here and would have rendered every day as empty.
  //
  // Then SHAPED through `normalizeRecords`, which is legacy's own playback
  // boot verbatim: `DATA=flowToRenderModel(RAW)` (viewer.html:3922) — drop the
  // `_type` meta lines, normalize the space-separated action spellings.
  // Reading the raw array left the flow file's leading `{"_type":"schema"}`
  // header in the set, where (having no `machine_uid`) it rendered a phantom
  // "unknown" machine card, an `Invalid Date other` log row, and a third
  // timeline lane. Deliberately NOT `buildFlowWindow`: that one also windows
  // to the last 24h of WALL-CLOCK, which for any day but today drops every
  // record in the file.
  const records = normalizeRecords(asRecordArray(query.data.data));

  if (records.length === 0) {
    return (
      <div data-state="empty">
        <div className="stagehdr">playback</div>
        <div className="none">no records for {date}.</div>
      </div>
    );
  }

  // tMax drives presence/staleness math inside the hero. For a replay it is the
  // last record of the DAY, not `Date.now()` — otherwise every card would be
  // measured against a clock the records never ran under. Uses the SHARED
  // `computeTMax` (`lib/flow.ts:140`, the same one `useFlowWindow` uses) rather
  // than a local reduce: an earlier draft here guessed the timestamp field as
  // `ts_ms` when it is `ts`, which would have silently produced tMax = 0.
  const tMax = computeTMax(records);
  // `tMin` is the replay timeline's LEFT edge (`recompute()`, viewer.html:1051
  // → `tlMin=liveMode?(tlMax-winMs):tMin`, :1727). Live mode derives its left
  // edge from the clock and never needs this.
  const tMin = computeTMin(records);

  return <FleetLens records={records} tMax={tMax} tMin={tMin} historical />;
}
