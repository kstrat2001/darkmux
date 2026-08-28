import { computeTMax, computeTMin } from "../../lib/flow";
import { getSource } from "../../lib/source";
import { useDay } from "../../hooks/useDay";
import { FleetLens } from "../fleet/FleetLens";
import type { FlowRecord } from "../../types/handwritten";

/** Legacy's own play-loop constants (viewer.html:2848-2860): a 100ms tick
 * advancing the playhead by the measured elapsed wall clock times the
 * labeled speed (`1h/s` by default). It used to be `(tMax-tMin)/120` per
 * tick, a fixed ~12s per day under a "1×" label (#2071 follow-up). */

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
 * (#1801, #2086) `date` is `string | null` — `null` exactly when `route.ts`
 * forced this route on a static build, which has one committed file and no
 * date until that file resolves. Either way the day comes from `useDay`
 * (`hooks/useDay.ts`): this lens has no fetch of its own.
 */
export function PlaybackLens({ date, playhead = null }: { date: string | null; playhead?: number | null }) {
  // (#2086) The day comes from the one resolver; this lens only renders it.
  const day = useDay(date);
  if (day.loading) {
    return (
      <div data-state="pending" role="status" aria-label={date ? `Loading ${date}` : "Loading playback"}>
        <div className="stagehdr">playback</div>
        <div className="none">loading…</div>
      </div>
    );
  }
  if (day.error) {
    return (
      <div data-state="error" role="alert">
        <div className="stagehdr">playback</div>
        <div className="none">
          couldn't reach /flow/{date}
          {day.error.status !== null ? ` (HTTP ${day.error.status})` : ""}: {day.error.message}
        </div>
      </div>
    );
  }
  const records = day.records ?? [];
  if (records.length === 0) {
    return (
      <div data-state="empty">
        <div className="stagehdr">playback</div>
        <div className="none">{getSource().kind === "static" ? "no records in the static playback source." : `no records for ${date}.`}</div>
      </div>
    );
  }
  return renderTransportStage(records);

  function renderTransportStage(dayRecords: FlowRecord[]) {
    // (#2071) The transport itself lives in the app shell's sticky block
    // now (`App.tsx`, `usePlaybackTransport`); this lens is a controlled
    // stage: it renders the day at the playhead the shell hands it. `tMax`
    // is the FIXED day ceiling (never moved by scrubbing) and `playhead` the
    // scrub position — see `timeline.ts`'s module doc for why the two must
    // stay separate.
    const tMax = computeTMax(dayRecords);
    const tMin = computeTMin(dayRecords);
    const playheadT = playhead ?? tMax;
    return <FleetLens records={dayRecords} tMax={tMax} tMin={tMin} playhead={playheadT} historical />;
  }
}
