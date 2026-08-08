import { useEffect, useRef, useState } from "react";
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
 *   - "live" → `location.hash = ""` (the `fleet` route). QA correction
 *     (post-first-review, must-fix): the module doc HERE used to call this
 *     an improvement "for a destination /next already owns" — that overstates
 *     it. `/next`'s default route OWNS THE ROUTE, not the render: today it's
 *     `FleetStrip` (a `/fleet/machines/live` presence strip), while legacy's
 *     `fleet.txt` goldens show a full dispatch/token hero + activity timeline
 *     that `/next` doesn't reproduce yet (`PlaybackLens.tsx`'s own doc names
 *     the same gap: the fleet-hero pipeline is Packet 5's territory). So this
 *     is a TEMPORARY narrowing, not a completed improvement — staying inside
 *     `/next` is still strictly better than a full-page bounce to legacy for
 *     a route this app already parses and will eventually render for real;
 *     it just isn't real YET. Once Packet 5 ports the fleet-hero pipeline,
 *     this becomes what the original wording claimed.
 *   - a mission row → `location.hash = "mission=<id>"`, reusing the
 *     `mission-redirect` route `route.ts` already recognizes — see
 *     `MissionReplay.tsx` for what that now does (real fetch + conditional
 *     navigation, replacing Packet 1's placeholder). This ONE destination
 *     really is fully real today (`/mission/<id>/graph` is a permanent,
 *     separate, already-working page — verified live against a real daemon,
 *     see the packet report).
 *   - a day row → `location.hash = "<date>"`, the NEW `playback` route this
 *     packet adds — see `PlaybackLens.tsx` for why that's a visible
 *     not-ported notice rather than a full historical render (legacy's OWN
 *     day-row click is a full navigation to `/play/<date>`, a server route
 *     this app doesn't reproduce in-SPA yet; ledgered as a follow-up there).
 *
 * Dismissal (QA must-fix — legacy has THREE ways to close `#catpanel`, this
 * component dropped all of them at first): ported both of legacy's handlers
 * verbatim as one `useEffect`, active only while `open` —
 *   - Escape (`viewer.html:3020-3023`): closes unconditionally.
 *   - click outside the panel AND outside the toggle
 *     (`viewer.html:3002-3008`): both live inside `.catalog-anchor` below, so
 *     one `!anchorRef.current.contains(e.target)` containment check covers
 *     both exclusions at once — a click on the toggle re-closes via the
 *     toggle's OWN `onClick` (which already flips `open`), never a second,
 *     conflicting close from this listener (the containment check is false
 *     for a click inside the anchor, so the effect no-ops on it either way).
 * Neither handler touches `#catpanel`'s own DOM/text, so byte-parity with
 * `catalog-open.txt` is unaffected.
 *
 * Geometry (QA must-fix, same review): `.catpanel` used to be
 * `position:absolute` at a page-level `top:48px;left:16px` — a magic
 * coordinate that, once `#meta` existed above it, overlapped the toggle
 * BUTTON itself (QA measured `elementFromPoint` at the button's center
 * returning the panel, and a real Playwright click timing out). Fixed by
 * making `.catalog-anchor` (wrapping both the toggle and the panel) the
 * positioning context, and the panel `top: calc(100% + 6px)` relative to
 * THAT anchor — the panel now always renders directly below the toggle's
 * own box, so it can never cover it, regardless of what other App-level
 * chrome (like `#meta`) sits above.
 */
export function CatalogPanel() {
  const [open, setOpen] = useState(false);
  const anchorRef = useRef<HTMLDivElement>(null);

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

  // viewer.html:3002-3008 (click outside) + viewer.html:3020-3023 (Escape).
  // Attached only while `open` — a closed panel has nothing to dismiss, and
  // re-attaching fresh on every open avoids a stale-closure `open` check
  // inside the handlers (the effect itself is the gate).
  useEffect(() => {
    if (!open) return;
    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") setOpen(false);
    }
    function handleClick(e: MouseEvent) {
      if (anchorRef.current && e.target instanceof Node && !anchorRef.current.contains(e.target)) {
        setOpen(false);
      }
    }
    document.addEventListener("keydown", handleKeyDown);
    document.addEventListener("click", handleClick);
    return () => {
      document.removeEventListener("keydown", handleKeyDown);
      document.removeEventListener("click", handleClick);
    };
  }, [open]);

  // Both fetches are gated on `open` (never fired until the operator asks
  // for history), and every open re-fetches — matching `toggleCatalog()`
  // itself, which never caches across opens.
  const pending = open && (!daysQuery.data || !missionsQuery.data);

  return (
    <div className="catalog-anchor" ref={anchorRef}>
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
    </div>
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
