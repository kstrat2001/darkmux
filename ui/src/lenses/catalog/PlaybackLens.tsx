import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import { asRecordArray, computeTMax, computeTMin, fetchStaticFlowRecords, normalizeRecords } from "../../lib/flow";
import { staticFlowSrc } from "../../lib/staticSource";
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
 *
 * (#1801) `date` is `string | null` — `null` exactly when `staticSource.ts`'s
 * `isStaticBuild()` forced this route (`route.ts`'s own doc on the widened
 * playback variant). In that case this component reads the committed
 * `.jsonl` (`staticFlowSrc()`) instead of `/flow/<date>`, sharing
 * `queryKeys.staticFlowSrc` with `useRouteRecords`' own static branch for
 * the same one-cache-slot reason the date-keyed branch already shares
 * `flowDate`. The static branch ignores `date` entirely rather than trying
 * to fetch `/flow/<date>` first and falling back — there is no daemon to
 * serve that path at all, and legacy's own flowSrc branch reads the
 * committed file unconditionally too (see `route.ts`'s doc on why a static
 * build's date hash doesn't request a different day).
 */
export function PlaybackLens({ date }: { date: string | null }) {
  const flowSrc = staticFlowSrc();

  const dayQuery = useQuery({
    queryKey: queryKeys.flowDate(date ?? ""),
    queryFn: () => fetchJson<unknown>(`/flow/${encodeURIComponent(date ?? "")}`),
    enabled: flowSrc === null,
  });
  const staticQuery = useQuery({
    queryKey: queryKeys.staticFlowSrc(flowSrc ?? ""),
    queryFn: () => fetchStaticFlowRecords(flowSrc ?? ""),
    enabled: flowSrc !== null,
  });

  if (flowSrc !== null) {
    // `fetchStaticFlowRecords` already collapses a network failure, a 404,
    // or a malformed file to `[]` (matching legacy's own silent catch — see
    // that function's own doc) — a static build has no daemon to report an
    // HTTP status FROM, so there is no separate error branch here the way
    // the date-keyed arm below has one.
    if (staticQuery.data === undefined) {
      return (
        <div data-state="pending" role="status" aria-label="Loading playback">
          <div className="stagehdr">playback</div>
          <div className="none">loading…</div>
        </div>
      );
    }

    // Shaped through the SAME `normalizeRecords` the date-keyed arm below
    // uses — drops the `_type` meta lines, normalizes action spellings. See
    // that arm's own comment for why this matters (the phantom "unknown"
    // machine card a raw read would otherwise produce).
    const records = normalizeRecords(staticQuery.data);

    if (records.length === 0) {
      return (
        <div data-state="empty">
          <div className="stagehdr">playback</div>
          <div className="none">no records in the static playback source.</div>
        </div>
      );
    }

    const tMax = computeTMax(records);
    const tMin = computeTMin(records);
    return <FleetLens records={records} tMax={tMax} tMin={tMin} historical />;
  }

  if (!dayQuery.data) {
    return (
      <div data-state="pending" role="status" aria-label={`Loading ${date}`}>
        <div className="stagehdr">playback</div>
        <div className="none">loading…</div>
      </div>
    );
  }

  if (!dayQuery.data.ok) {
    return (
      <div data-state="error" role="alert">
        <div className="stagehdr">playback</div>
        <div className="none">
          couldn't reach /flow/{date}
          {dayQuery.data.status !== null ? ` (HTTP ${dayQuery.data.status})` : ""}: {dayQuery.data.message}
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
  const records = normalizeRecords(asRecordArray(dayQuery.data.data));

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
