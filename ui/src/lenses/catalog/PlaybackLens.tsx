import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import { asRecordArray, computeTMax, computeTMin, fetchStaticFlowRecords, normalizeRecords } from "../../lib/flow";
import { staticFlowSrc } from "../../lib/staticSource";
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
 *
 * (#1869) This component OWNS the playback transport's `t` (playhead)
 * state — `play`/`rewind`/`speed`/the scrub `<input type=range>` all live
 * here, not in `FleetLens` or `Scrubber` (a pure controlled view). `t` is
 * threaded down as `FleetLens`'s `playhead` prop — a NEW, separate prop
 * from `tMax`, not a reuse of the argument this component already passed.
 * The first cut reused `tMax` for both the day's fixed ceiling and the
 * scrub position (they were always the same number pre-transport, so
 * nothing distinguished them), and that conflation broke live: rewinding to
 * a day's start collapsed the activity axis itself down to a single
 * instant instead of staying fixed while the playhead marker swept back
 * across it. `tMax` here stays `computeTMax(dayRecords)` — the day's true,
 * FIXED ceiling, exactly as it was before this packet; `playhead` is the
 * new thing. `tMin` is unchanged either way. See `FleetLens.tsx`'s own
 * `playhead` prop doc and `timeline.ts`'s module doc for the fuller
 * account. One implementation, shared by the daemon's `#<date>` route and
 * the static demo build, by construction — both branches below call the
 * same
 * `renderTransportStage` closure.
 *
 * (#2071) The transport (play/pause, scrubber, tick loop) moved to the app
 * shell (`usePlaybackTransport`), which hands this lens the `playhead` it
 * renders at; nothing here owns time any more.
 * (#1869 code review, historical) `onPlayheadChange` reported the resolved playhead
 * (`playheadT`, the same value `Scrubber` and `FleetLens`'s `scopedData`
 * already read) up to `App`, which threads it into `EventLogColumn` — a
 * SIBLING of this whole lens in the DOM, not a descendant, so it can't
 * reach it any other way (see `App.tsx`'s own `eventLogRecords` doc).
 * Optional: every test in `PlaybackLens.test.tsx` mounts this component
 * standalone with no callback, which is fine — the reporter below no-ops
 * when it isn't given one.
 */
export function PlaybackLens({ date, playhead = null }: { date: string | null; playhead?: number | null }) {
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

  // `null` means "follow the newest record" — `tMax`, the pre-#1869 pinned
  // behavior, and the default the transport resets to below. Kept nullable
  // (rather than defaulting to `tMax` at init time) because `tMax` isn't
  // known until the day's records resolve, and holding a real number here
  // before that would need reconciling against a `tMax` that keeps changing
  // while the query is still pending.

  // `App.tsx` never remounts this component across a date change (same
  // component type, same position in the tree, only the `date` prop moves)
  // — so without this, scrubbing day A's records and then navigating to day
  // B would carry day A's playhead/playing/speed straight into a day whose
  // `tMin`/`tMax` it was never computed against.

  // The actual play-loop tick lives in `PlayLoop`, below — it needs the
  // day's `tMin`/`tMax`, which aren't known until records resolve (deep
  // inside `renderTransportStage`), so it's mounted from there rather than
  // as an effect at this level.

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

    return renderTransportStage(records);
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

  return renderTransportStage(records);

  /**
   * Shared by both branches above — the ONE place `t`/`playing`/`speed`
   * meet the day's actual `tMin`/`tMax` and get rendered as a real
   * transport + the fleet hero it drives. Defined inside the component (not
   * hoisted to module scope) so it closes over this render's `t`/`setT`/
   * `playing`/`setPlaying`/`speed`/`setSpeed` — a plain function, not
   * another component, so no extra render boundary or key is needed between
   * this and its two call sites.
   *
   * `tMax`/`tMin` come from `computeTMax`/`computeTMin` over records, EXACTLY
   * as they did pre-#1869 (this function doesn't change what those are —
   * only what the PLAYHEAD passed to `FleetLens` is allowed to be relative
   * to them).
   */
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
