import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import type { FlowDay, FlowDaysResponse, FlowMissionSummary, FlowMissionsResponse } from "../../types/handwritten";
import { CATALOG_MISSION_CAP, daySummary, missionSummary, missionsHeader, todayUTC } from "./format";

/**
 * The playback catalog (`#691`) — `viewer.html`'s `toggleCatalog()`/
 * `#catpanel`: a global, cross-lens history browser (every day with a flow
 * file, plus the cross-day mission rollup), reached via a "browse history"
 * toggle rather than a hash route of its own. Mounted at the App level
 * (`App.tsx`, a sibling of `#stage`), matching `#catpanel`'s own DOM
 * placement — a body-level overlay, not part of any one lens's render
 * target (see the parity harness's `extract-lens.js`'s
 * `extractCatalogText`, added this packet specifically because `#catpanel`
 * sits OUTSIDE the four regions the harness captured through Packet 3).
 *
 * `.catrow`'s `display:block` in `styles.css` (overriding `<button>`'s
 * inline-block UA default) is load-bearing for `next-parity-catalog.spec.ts`'s
 * byte-comparison against `goldens/catalog-open.txt`, same load-bearing-CSS
 * note as the runs lens's `.labrunmain`/`.runchip` — don't "clean up" it
 * without re-running that spec.
 *
 * Row-click destinations, per `route.ts`'s existing grammar rather than
 * legacy's raw `location.href` jumps (see that file's own module doc for the
 * precedent this reuses):
 *   - "live" → `location.hash = ""` (the `fleet` route, already fully real —
 *     a deliberate, judged IMPROVEMENT over legacy's `location.href="/"`,
 *     which would bounce the operator back to the classic viewer for a
 *     destination `/next` already owns; see the packet report).
 *   - a mission row → `location.hash = "mission=<id>"`, reusing the
 *     `mission-redirect` route `route.ts` already recognizes — see
 *     `MissionReplay.tsx` for what that now does (real fetch + conditional
 *     navigation, replacing Packet 1's placeholder).
 *   - a day row → `location.hash = "<date>"`, the NEW `playback` route this
 *     packet adds — see `PlaybackLens.tsx` for why that's a visible
 *     not-ported notice rather than a full historical render (legacy's OWN
 *     day-row click is a full navigation to `/play/<date>`, a server route
 *     this app doesn't reproduce in-SPA yet; ledgered as a follow-up there).
 */
export function CatalogPanel() {
  const [open, setOpen] = useState(false);

  const daysQuery = useQuery({
    queryKey: queryKeys.flowDays(),
    queryFn: () => fetchJson<FlowDaysResponse>("/flow-days"),
    enabled: open,
  });
  const missionsQuery = useQuery({
    queryKey: queryKeys.flowMissions(),
    queryFn: () => fetchJson<FlowMissionsResponse>("/flow-missions"),
    enabled: open,
  });

  function goLive() {
    location.hash = "";
    setOpen(false);
  }
  function goDay(date: string) {
    location.hash = date;
    setOpen(false);
  }
  function goMission(id: string) {
    location.hash = `mission=${encodeURIComponent(id)}`;
    setOpen(false);
  }

  // Both fetches are gated on `open` (never fired until the operator asks
  // for history), and every open re-fetches — matching `toggleCatalog()`
  // itself, which never caches across opens.
  const pending = open && (!daysQuery.data || !missionsQuery.data);

  return (
    <>
      <button
        type="button"
        className="catalog-toggle"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        aria-controls="catpanel"
        title="browse history"
      >
        browse history
      </button>
      {open && (
        <div className="catpanel" id="catpanel">
          {pending ? (
            <div className="cathdr">loading…</div>
          ) : (
            <CatalogContent
              days={daysQuery.data?.ok ? daysQuery.data.data.days : []}
              missions={missionsQuery.data?.ok ? missionsQuery.data.data.missions : []}
              onLive={goLive}
              onDay={goDay}
              onMission={goMission}
            />
          )}
        </div>
      )}
    </>
  );
}

/** `viewer.html`'s `toggleCatalog()` content-build, ported to JSX: the
 * unconditional live row, the mission section (only when non-empty, capped
 * + disclosed via `missionsHeader`), then either the day rows or the
 * `.catempty` fallback. A failed fetch on either side (a non-2xx/network
 * error via `fetchJson`) is treated the same as legacy's
 * `Promise.allSettled` catch-swallow: an empty array for that section, not a
 * distinct error state — this panel is a convenience browser over data that
 * degrades gracefully, not a load-bearing data view (Rule 1, pure port
 * including its silences). */
function CatalogContent({
  days,
  missions,
  onLive,
  onDay,
  onMission,
}: {
  days: FlowDay[];
  missions: FlowMissionSummary[];
  onLive: () => void;
  onDay: (date: string) => void;
  onMission: (id: string) => void;
}) {
  const today = todayUTC();
  return (
    <>
      <div className="cathdr">playback catalog</div>
      <button type="button" className="catrow live" onClick={onLive}>
        <div className="cd">● live · today</div>
        <div className="cs">now-ish on present machines</div>
      </button>
      {missions.length > 0 && (
        <>
          <div className="cathdr">{missionsHeader(missions.length, CATALOG_MISSION_CAP)}</div>
          {missions.slice(0, CATALOG_MISSION_CAP).map((m) => (
            <button type="button" className="catrow" key={m.mission_id} onClick={() => onMission(m.mission_id)}>
              <div className="cd">▣ {m.mission_id}</div>
              <div className="cs">{missionSummary(m)}</div>
            </button>
          ))}
        </>
      )}
      {days.length > 0 ? (
        <>
          <div className="cathdr">days</div>
          {days.map((d) => (
            <button type="button" className="catrow" key={d.date} onClick={() => onDay(d.date)}>
              <div className="cd">
                {d.date}
                {d.date === today ? " · today" : ""}
              </div>
              <div className="cs">{daySummary(d)}</div>
            </button>
          ))}
        </>
      ) : (
        <div className="catempty">no recorded days yet — dispatches you run will appear here.</div>
      )}
    </>
  );
}
