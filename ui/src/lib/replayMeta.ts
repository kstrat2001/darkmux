/**
 * The REPLAY arm of `renderMeta()` (viewer.html:1308-1341) and the mission
 * helpers it shares with the crumb — `liveScopedMissions()` /
 * `primaryMission()` / `missionLabel()` (viewer.html:1207-1221).
 *
 * `lib/metaLine.ts` is the LIVE arm and stays untouched. Legacy branches
 * these two apart explicitly, and the split is not cosmetic:
 *
 *   live:   `${head}`
 *   replay: `${head} · <b>${DATA_SOURCE}</b> · ${lday(tMin)} ${clk(tMin)}–${clk(tMax)}`
 *           `<br>${DATA.length} records · ${machines().length} machines`
 *
 * with legacy's own reason recorded at the branch: a live view is a ROLLING
 * window, so naming raw clock endpoints reads as a backwards range past local
 * midnight, and the record count already appears on the event pane's counter
 * chip. A replay is a SPECIFIC DAY — its date and span are the context, and
 * its census has nowhere else to live.
 *
 * (#1800) `/next` computed `#meta` from the live rolling window on EVERY
 * route, so a replay's status bar described today. That was named as a
 * deferral rather than fixed while `playback` rendered a placeholder; once
 * P2 made the stage real it became the loudest of the three surfaces
 * disagreeing about what day the page was showing.
 */

import { T, uidOf } from "./flow";
import { clk, lday } from "./format";
import type { FlowRecord } from "../types/handwritten";

/** `missions` — `recompute()`, viewer.html:1052. Distinct `mission_id`s in
 * record order, which is timestamp order, so "first" means oldest. */
export function missionIds(data: FlowRecord[]): string[] {
  return [...new Set(data.filter((r) => r.mission_id).map((r) => r.mission_id as string))];
}

/** `liveScopedMissions()` — viewer.html:1207-1214, REPLAY arm only.
 *
 * The live arm filters to missions with a currently-running session, and
 * returns empty when nothing is running — which is why `goldens/fleet.txt`
 * has an EMPTY crumb while `goldens/playback-date.txt` shows a mission. A
 * replay is not presence-scoped (the live session set is empty there by
 * construction), so it shows the window's missions. Callers on the live path
 * already have `metaLine.ts`; this module is only ever reached from a replay,
 * so the live arm is deliberately absent rather than reimplemented here.
 */
export function replayMissions(data: FlowRecord[]): string[] {
  return missionIds(data);
}

/** `primaryMission()` — viewer.html:1216. The single mission the crumb
 * focuses on. */
export function primaryReplayMission(data: FlowRecord[]): string | null {
  const ms = replayMissions(data);
  return ms.length ? ms[0] : null;
}

/** `missionLabel()` — viewer.html:1217-1221. Caps at two named missions plus
 * a "+N more" so a busy day's headline cannot sprawl. `—` (an em dash) when
 * the day has no missions at all, which is a real state: a day of unscoped
 * `dispatch` calls has records and no mission ids. */
export function replayMissionLabel(data: FlowRecord[]): string {
  const ms = replayMissions(data);
  if (!ms.length) return "—";
  return ms.length > 2 ? `${ms.slice(0, 2).join(", ")} +${ms.length - 2} more` : ms.join(", ");
}

/** `setBadges()`'s `DATA_SOURCE` — viewer.html:3465, the `play` arm. Rendered
 * INSIDE the meta line (legacy wraps it in `<b>`), distinct from `#srcbadge`'s
 * own text, which is capitalized differently ("Flow · <date>"). Two strings,
 * two places, and they do not match — so neither is derived from the other. */
export function replayDataSource(date: string): string {
  return `flow · ${date}`;
}

/** The two `#meta` lines for a replay, in legacy's order. Split on the `<br>`
 * rather than joined, matching `computeMetaLines`'s existing shape so `App`
 * renders both arms through the same two `<div>`s.
 *
 * `machineCount` is `machines().length` — passed in rather than recomputed
 * because the caller already unions the record uids with (on a live route)
 * the presence map, and a second implementation here would be free to drift
 * from the one the fleet cards are built from. On a replay the presence map
 * is empty, so this is just the record uids. */
export function replayMetaLines(data: FlowRecord[], date: string): string[] {
  const ts = data.map((r) => T(r.ts)).filter((n) => !Number.isNaN(n));
  const tMin = ts.length ? Math.min(...ts) : Date.now();
  const tMax = ts.length ? Math.max(...ts) : Date.now();
  const machineCount = new Set(data.map(uidOf)).size;
  // The literal space before the clock range is legacy's own: the template
  // ends `${lday(tMin)} ` and the next line begins `${clk(tMin)}`.
  const head = `◆ ${replayMissionLabel(data)}`;
  return [
    `${head} · ${replayDataSource(date)} · ${lday(tMin)} ${clk(tMin)}–${clk(tMax)}`,
    `${data.length} records · ${machineCount} machines`,
  ];
}
