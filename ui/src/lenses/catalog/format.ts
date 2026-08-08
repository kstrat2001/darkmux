import type { FlowDay, FlowMissionSummary } from "../../types/handwritten";

/**
 * Pure formatting functions ported from `viewer.html`'s `toggleCatalog()`
 * (search that file for `(#691) Playback catalog day-picker`) — the exact
 * per-row summary strings the catalog panel renders, split out for unit
 * testing the same way `lenses/runs/format.ts` does. `CatalogPanel.tsx`
 * composes these into JSX; nothing here touches the DOM.
 */

/** `viewer.html`'s own mission-list cap (`CATALOG_MISSION_CAP`, #1569
 * sweep) — a cap that says nothing reads as "this is all of them", so the
 * header text below always discloses when it's truncating. */
export const CATALOG_MISSION_CAP = 50;

/** `viewer.html`'s `todayUTC()` — today's date, UTC, `YYYY-MM-DD`. Reads the
 * ambient clock (real or Playwright's frozen one — both are just `Date` in
 * the same JS realm), matching legacy exactly rather than threading a
 * parameter through every caller. */
export function todayUTC(): string {
  return new Date().toISOString().slice(0, 10);
}

/** `viewer.html`: the day row's `.cs` text —
 * `` `${disp} dispatch${..} · ${recs} record${..}${missTxt}` ``, where
 * `missTxt` previews up to 3 of the day's own mission ids (`, `-joined,
 * `+N` suffix beyond 3). */
export function daySummary(day: FlowDay): string {
  const disp = day.dispatches || 0;
  const recs = day.records || 0;
  const ms = day.missions || [];
  const missTxt = ms.length
    ? ` · ${ms.length === 1 ? "mission" : "missions"} ${ms.slice(0, 3).join(", ")}${ms.length > 3 ? ` +${ms.length - 3}` : ""}`
    : "";
  return `${disp} dispatch${disp === 1 ? "" : "es"} · ${recs} record${recs === 1 ? "" : "s"}${missTxt}`;
}

/** `viewer.html`: the mission row's `.cs` text —
 * `` `${disp} dispatch${..} · ${recs} record${..}${span}` ``, where `span`
 * is a date range (`first_date`–`last_date`) when the mission crossed a day
 * boundary, or just the one date otherwise. */
export function missionSummary(m: FlowMissionSummary): string {
  const disp = m.dispatches || 0;
  const recs = m.records || 0;
  const span =
    m.first_date && m.first_date !== m.last_date ? `${m.first_date}–${m.last_date}` : m.last_date || m.first_date || "";
  return `${disp} dispatch${disp === 1 ? "" : "es"} · ${recs} record${recs === 1 ? "" : "s"}${span ? ` · ${span}` : ""}`;
}

/** `viewer.html`: the missions section's `.cathdr` text — plain "missions"
 * unless the list is truncated, in which case it names the cap (#1569
 * sweep's cap-disclosure, so an absent mission reads as "not in the newest
 * N" rather than silently missing). */
export function missionsHeader(total: number, cap: number = CATALOG_MISSION_CAP): string {
  return total > cap ? `missions · newest ${cap} of ${total}` : "missions";
}
