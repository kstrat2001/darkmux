/**
 * The fleet default view's "recent activity" timeline — `renderFleet()`'s
 * `lanes`/`ax`/`winCtl`/`tl` build (viewer.html:1688-1741). One lane per
 * machine; each dispatch session is a bar positioned across the recorded
 * window.
 *
 * (#1151) Two anchorings, and which one applies is the whole of this file's
 * `liveMode` argument:
 *
 * - **Live**: `tlMax = Math.max(tMax, nowMs)`, `tlMin = tlMax - window`. NOW,
 *   not the newest record — after a run ends `tMax` stops advancing while the
 *   clock does not, so a `tMax` anchor would read 16:00 at 16:04. "24h" draws
 *   a TRUE 24h axis ending at the current minute, and the operator picks the
 *   window from the 10m/1h/4h/24h control.
 * - **Replay**: `tlMin..tlMax = tMin..tMax`, the recorded day's own span, and
 *   NO window control (there is nothing to slide over — the dataset is the
 *   window). The header also drops "recent", because the day is not recent.
 *
 * (#1800 P2) The live arm used to be the only arm, since `/next` had no
 * historical route to reach the other one. `Math.max(tMax, nowMs)` on a
 * replayed day is `nowMs` by definition, which drew a 2026-08-07 page with an
 * "AUG 12–AUG 13" axis and zero bars — every bar fell before `tlMin` and was
 * dropped by the window filter below.
 *
 * `state.t` (the playhead) is `tMax` in both modes — this port has no
 * scrubber; see `savings.ts`'s module doc for the same reasoning applied to
 * the token sums.
 */

import {
  T,
  sessionsOn,
  dispatchRec,
  dispatchEnd,
  dispatchErrored,
  dispatchKilled,
  sessEnd,
  sessionRunning,
  statusLabel,
  lastTs,
  nameOf,
} from "../../lib/flow";
import { clkhm, clkrange } from "../../lib/format";
import type { FlowRecord, PresenceBeat } from "../../types/handwritten";

/** The live-only window presets (#1151) — minutes, matching legacy's
 * `[{l:'10m',m:10},{l:'1h',m:60},{l:'4h',m:240},{l:'24h',m:1440}]` verbatim.
 * `label` stays lowercase — `.twinb`'s `text-transform:uppercase` (ported
 * into `styles.css`) renders it, same as legacy's own CSS-driven casing. */
export const ACTIVITY_WINDOW_PRESETS: { label: string; minutes: number }[] = [
  { label: "10m", minutes: 10 },
  { label: "1h", minutes: 60 },
  { label: "4h", minutes: 240 },
  { label: "24h", minutes: 1440 },
];

export const DEFAULT_ACTIVITY_WINDOW_MIN = 1440;

export interface TimelineBar {
  sid: string;
  leftPct: number;
  widthPct: number;
  cls: "run" | "done" | "canceled" | "err";
  title: string;
}

export interface TimelineLane {
  uid: string;
  name: string;
  bars: TimelineBar[];
}

export interface ActivityTimeline {
  /** `recent activity · <clkrange>` — lowercase; `.tlhdr`'s CSS
   * `text-transform: uppercase` renders it, matching legacy. */
  headerText: string;
  lanes: TimelineLane[];
  axis: [string, string, string];
  playheadPct: number;
  labelWidthPx: number;
}

/** `renderMachine()`'s lane-label width math (viewer.html:1733-1734) — sizes
 * the `.lname` column to the longest machine name so short names don't leave
 * a fixed gap. Visual-only (no text-parity effect). */
function labelWidthPx(uids: string[], data: FlowRecord[], liveMachines: Map<string, PresenceBeat>): number {
  const maxLen = uids.length ? Math.max(...uids.map((m) => nameOf(data, liveMachines, m).length)) : 8;
  return Math.round(Math.min(170, Math.max(54, maxLen * 7.4 + 10)));
}

export function buildActivityTimeline(
  data: FlowRecord[],
  liveMachines: Map<string, PresenceBeat>,
  uids: string[],
  liveSet: Set<string>,
  tMax: number,
  nowMs: number,
  windowMinutes: number,
  /** viewer.html:1727's `liveMode` — see this module's own doc for the two
   * anchorings and why a replay must not use the live one. */
  liveMode = true,
  /** The dataset's earliest timestamp (`recompute()`'s `tMin`). Read only in
   * replay, where it IS the left edge; ignored in live mode, whose left edge
   * is `tlMax - window`. */
  tMin = 0,
): ActivityTimeline {
  const winMs = windowMinutes * 60000;
  const tlMax = liveMode ? Math.max(tMax, nowMs) : tMax;
  const tlMin = liveMode ? tlMax - winMs : tMin;
  const span = Math.max(1, tlMax - tlMin);
  const pct = (t: number) => ((t - tlMin) / span) * 100;

  const lanes: TimelineLane[] = uids.map((m) => {
    const bars: TimelineBar[] = [];
    for (const sid of sessionsOn(data, m)) {
      const s = dispatchRec(data, sid, "start");
      if (!s) continue;
      const term = dispatchEnd(data, sid);
      const e = sessEnd(data, sid);
      const closeCands = [term ? T(term.ts) : null, e ? T(e.ts) : null].filter((x): x is number => x != null);
      const closeTs = closeCands.length ? Math.min(...closeCands) : null;
      // (#857) `done` = not currently running, through the SHARED
      // `sessionRunning` — live keys on presence, replay on the close-edge at
      // the playhead. (#1800 P2: this was `!liveSet.has(sid)`, the live arm
      // inlined, which read every session of a replayed day as running.)
      const done = !sessionRunning(data, liveSet, sid, liveMode, tMax);
      const errored = done && dispatchErrored(term);
      const killed = dispatchKilled(term);
      const clean = done && !!term && !dispatchErrored(term);
      const lbl = statusLabel({ open: !done, errored, killed, clean });
      const cls: TimelineBar["cls"] = !done ? "run" : errored ? "err" : clean ? "done" : "canceled";
      const end = !done ? tMax : closeTs != null ? closeTs : lastTs(data, sid) || tMax;
      if (end < tlMin) continue; // ended entirely before the window
      const cst = Math.max(T(s.ts), tlMin); // clip a straddling start to the window edge
      const widthPct = Math.max(0.6, pct(end) - pct(cst));
      const leftPct = Math.max(0, Math.min(pct(cst), 100 - widthPct)); // never spill past the right edge
      const role = (s.handle || "").replace(/^darkmux\//, "");
      bars.push({ sid, leftPct, widthPct, cls, title: `${role} · ${sid} · ${lbl}` });
    }
    return { uid: m, name: nameOf(data, liveMachines, m), bars };
  });

  return {
    // `${liveMode?'recent activity':'activity'} · ${clkrange(tlMin,tlMax)}`
    // — viewer.html:1766. Lowercase; `.tlhdr`'s `text-transform: uppercase`
    // renders it, matching legacy.
    headerText: `${liveMode ? "recent activity" : "activity"} · ${clkrange(tlMin, tlMax)}`,
    lanes,
    axis: [clkhm(tlMin), clkhm(tlMin + span / 2), clkhm(tlMax)],
    playheadPct: pct(tMax),
    labelWidthPx: labelWidthPx(uids, data, liveMachines),
  };
}
