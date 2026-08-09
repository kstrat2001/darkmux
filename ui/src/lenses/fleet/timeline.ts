/**
 * The fleet default view's "recent activity" timeline — `renderFleet()`'s
 * `lanes`/`ax`/`winCtl`/`tl` build (viewer.html:1688-1741). One lane per
 * machine; each dispatch session is a bar positioned across the recorded
 * window.
 *
 * (#1151) This port has no playback scrubber, so it's always the live-only
 * branch: `tlMax = Math.max(tMax, nowMs)` (anchored at NOW so the right-edge
 * axis label stays honest after the last record — see the legacy comment
 * this ports), `tlMin = tlMax - windowMinutes*60000`. `state.t` (the
 * playhead) is always `tMax` here — see `savings.ts`'s module doc for the
 * same reasoning applied to the token sums.
 */

import {
  T,
  sessionsOn,
  dispatchRec,
  dispatchEnd,
  dispatchErrored,
  dispatchKilled,
  sessEnd,
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
): ActivityTimeline {
  const winMs = windowMinutes * 60000;
  const tlMax = Math.max(tMax, nowMs);
  const tlMin = tlMax - winMs;
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
      // (#857) `done` is "not currently live" — this port's live-mode-only
      // `sessionRunning()`.
      const done = !liveSet.has(sid);
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
    headerText: `recent activity · ${clkrange(tlMin, tlMax)}`,
    lanes,
    axis: [clkhm(tlMin), clkhm(tlMin + span / 2), clkhm(tlMax)],
    playheadPct: pct(tMax),
    labelWidthPx: labelWidthPx(uids, data, liveMachines),
  };
}
