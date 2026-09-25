/**
 * The fleet default view's "recent activity" timeline — `renderFleet()`'s
 * `lanes`/`ax`/`winCtl`/`tl` build (viewer.html:1688-1741). One lane per
 * machine; each dispatch session is a bar positioned across the recorded
 * window.
 *
 * (Playback parity, Change A, finding #8, 2026-09-24) Used to be TWO
 * anchorings selected by `liveMode` — live drew a rolling window ending at
 * `Math.max(tMax, nowMs)`; replay drew the recorded day's own fixed span
 * with no window control, headed "activity" instead of "recent activity".
 * That is a parity defect, not a feature (operator decision): a replay now
 * draws the SAME rolling window as live, `tlMax = playheadT` unconditionally
 * (live or replayed), `tlMin = tlMax - window`, same header, same 10m/1h/
 * 4h/24h control. The day's own whole span lives only in the scrubber now.
 * `playheadT` is the caller's one clock — `playhead ?? wallNow` — so at the
 * live edge this is "now" (not the newest record's timestamp: see
 * `FleetLens.tsx`'s own doc on the Side finding this also fixes), and under
 * scrub it is the playhead, advancing with it during playback exactly as
 * live's window advances with the wall clock.
 *
 * (#1800 P2, historical) The live arm used to be the only arm, since `/next`
 * had no historical route to reach the other one. `Math.max(tMax, nowMs)` on
 * a replayed day was `nowMs` by definition, which drew a 2026-08-07 page
 * with an "AUG 12–AUG 13" axis and zero bars — every bar fell before `tlMin`
 * and was dropped by the window filter below. The per-mode branch this doc
 * used to describe is what finding #8 above then found to be its own,
 * different defect, one layer up.
 *
 * (#1869) `state.t` (the playhead) and `tMax` (the day's fixed ceiling) are
 * TWO SEPARATE VALUES in legacy — `tMax` is set once by `recompute()` at
 * boot and never moves; `state.t` is what the scrubber drags around. This
 * port's `tMax` PARAMETER used to serve both roles at once, silently,
 * because before the playback transport existed nothing ever hands this
 * function a `state.t` that differs from `tMax` — every caller was always
 * pinned at the ceiling, so the conflation was invisible. It stopped being
 * invisible against a real daemon: rewinding to the start of a day made
 * `tlMax` (still fed from the SAME argument) collapse to `tlMin`, and the
 * activity axis read "16:56–16:56" instead of showing the day's whole span
 * with the playhead marker swept back to its left edge — exactly the
 * conflation this doc now separates out.
 *
 * So this function keeps `tMax` as the axis CEILING (`tlMax` in replay is
 * still `tMax`, unmoved by scrubbing) and takes a SEPARATE `playheadT`
 * parameter (defaulting to `tMax`, so every existing caller — anything that
 * never had a scrubber to begin with — is unaffected) for everything that
 * legacy keys on `state.t`: the bar loop's "not started yet" guard,
 * `sessionRunning`'s close-edge comparison, an open bar's `end`, and
 * `playheadPct`. `FleetLens` is the caller that now passes these as two
 * genuinely different numbers on a replay route (see its own doc for the
 * `tMax`/`playhead` prop split this traces back to).
 *
 * The bar loop's "not started yet" guard restores legacy's own
 * `bars=sessionsOn(m).map(sid=>{const s=dispatch(sid,"start");
 * if(!s||T(s.ts)>state.t)return""; ...})` — a session that hasn't started
 * yet as of the PLAYHEAD (not the axis ceiling) must not draw a bar at all;
 * without it, `sessionRunning` finds no close-edge for it (there's nothing
 * to close yet) and defaults to "running", drawing a phantom sliver. See
 * `savings.ts`'s module doc for the parallel restoration applied to the
 * token sums (a caller-side gate, not a change to this file).
 */

import {
  T,
  sessionRunsOn,
  dispatchRec,
  dispatchEnd,
  dispatchErrored,
  dispatchKilled,
  sessEnd,
  sessionRunning,
  statusLabel,
  runStateFrom,
  lastTs,
  nameOf,
} from "../../lib/flow";
import { clkhm } from "../../lib/format";
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
  /** The session id — still the click-through target (`#dispatch=<sid>`,
   * `FleetLens.tsx`) and the `data-arg` shown to the operator, unchanged.
   * NOT guaranteed unique within a lane on its own (#2125) — a review
   * mission's reused step session id can produce two bars sharing this
   * value, one per mission; use `key` for anything requiring uniqueness. */
  sid: string;
  /** (#2125) `sid` plus its mission id when it has one — the actual unique
   * identity of ONE bar. Always distinct across bars in the same lane,
   * unlike `sid` alone. Use this for React `key`s / dedup, never `sid`. */
  key: string;
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
  /** `recent activity` — lowercase; `.tlhdr`'s CSS `text-transform:
   * uppercase` renders it.
   *
   * (operator, 2026-09-01) The `· <clkrange>` suffix is GONE. It wrapped to
   * two lines on a phone to restate what two other surfaces already say: the
   * axis under the lanes carries the times and updates live, and the masthead
   * chip carries the day. A heading that wraps in order to repeat its own
   * neighbours is spending the scarcest thing on screen. */
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
  /** The axis CEILING — kept as a parameter for `playheadT`'s default
   * expression below (so every pre-existing caller that only ever passed
   * one clock value is unaffected), but no longer read for anything else.
   * See this module's own doc, Playback parity Change A. */
  tMax: number,
  /** (Playback parity, Change A) No longer read — the rolling window is
   * always anchored at `playheadT` now (`playhead ?? wallNow`, computed by
   * the caller), in both modes. Kept as a parameter only so existing call
   * sites don't need a positional rewrite. */
  _nowMs: number,
  windowMinutes: number,
  /** (Playback parity, Change A, finding #8) No longer read — see this
   * module's own doc for why the day-span/no-window-control replay arm was
   * a parity defect, not a feature: a replay now draws the SAME rolling
   * window as live, anchored at the playhead, with the same header and the
   * same window control. Kept as a parameter only so existing call sites
   * don't need a positional rewrite. */
  _liveMode = true,
  /** (Playback parity, Change A, finding #8) No longer read — the day's own
   * span now lives only in the scrubber (operator decision); this
   * function's own left edge is always `tlMax - window`. Kept as a
   * parameter only so existing call sites don't need a positional rewrite. */
  _tMin = 0,
  /** (#1869) The PLAYHEAD — `state.t` in legacy terms, a genuinely separate
   * value from `tMax` once a replay can scrub. Defaults to `tMax` so every
   * caller that predates the transport (live mode; any test that only ever
   * passed one number) keeps its exact prior behavior — playhead == ceiling,
   * unconditionally. See this module's own doc for the bug this default
   * exists to NOT reproduce when a real caller passes something else.
   *
   * (Playback parity, Change A) This is now the ONE clock the whole
   * function anchors on — `tlMax = playheadT` unconditionally, live or
   * replayed. The caller computes it as `playhead ?? wallNow`, so at the
   * live edge this is "now" (not the newest record's timestamp — see
   * `FleetLens.tsx`'s own doc on the Side finding this fixes), and under
   * scrub it is the playhead. */
  playheadT = tMax,
  /** (#2890) A replay's "all" window: the axis is the recording's own
   *  [start, end], fixed, instead of a window rolling back from the
   *  playhead. Bars still stop at the playhead, so the lanes fill in as it
   *  plays. Absent (every live call, and a replay with a preset picked)
   *  keeps the rolling window. */
  fixedRange?: [number, number],
): ActivityTimeline {
  const winMs = windowMinutes * 60000;
  const tlMax = fixedRange ? fixedRange[1] : playheadT;
  const tlMin = fixedRange ? fixedRange[0] : tlMax - winMs;
  const span = Math.max(1, tlMax - tlMin);
  const pct = (t: number) => ((t - tlMin) / span) * 100;

  const lanes: TimelineLane[] = uids.map((m) => {
    const bars: TimelineBar[] = [];
    // (#2125) `sessionRunsOn` — NOT `sessionsOn` — yields one entry per
    // (session_id, mission_id) pair, not per bare session_id. A review
    // mission's per-step session id (`task-review-probe-mid-task` etc) is a
    // FIXED string reused by every review run; two DIFFERENT missions
    // sharing one entry here would let `dispatchRec`/`dispatchEnd`/`sessEnd`
    // below (unscoped `Array.find`) pair one mission's start with a
    // DIFFERENT mission's terminal/abort — measured live as a single
    // 20-hour "canceled" span for a mission that actually ran 23 minutes.
    // `missionId` threaded through every lookup below scopes each one to
    // its OWN mission's records; `undefined` (a session with no mission at
    // all) preserves the exact prior session-id-only behavior.
    for (const { sessionId: sid, missionId } of sessionRunsOn(data, m)) {
      const s = dispatchRec(data, sid, "start", missionId);
      // (#1869) `T(s.ts) > playheadT` — restores legacy's
      // `if(!s||T(s.ts)>state.t)return"";`. A session that hasn't started
      // yet as of the PLAYHEAD (not the axis ceiling) must not draw a bar at
      // all; without it, `sessionRunning` finds no close-edge for it
      // (there's nothing to close) and defaults to "running", drawing a
      // phantom sliver at the track's right edge.
      if (!s || T(s.ts) > playheadT) continue;
      const term = dispatchEnd(data, sid, missionId);
      const e = sessEnd(data, sid, missionId);
      const closeCands = [term ? T(term.ts) : null, e ? T(e.ts) : null].filter((x): x is number => x != null);
      const closeTs = closeCands.length ? Math.min(...closeCands) : null;
      // (#857) `done` = not currently running, through the SHARED
      // `sessionRunning` — live keys on presence, replay on the close-edge at
      // the playhead. (#1800 P2: this was `!liveSet.has(sid)`, the live arm
      // inlined, which read every session of a replayed day as running.)
      const done = !sessionRunning(data, liveSet, sid, playheadT, missionId);
      const errored = done && dispatchErrored(term);
      const killed = dispatchKilled(term);
      const clean = done && !!term && !dispatchErrored(term);
      const lbl = statusLabel(runStateFrom({ open: !done, errored, killed, clean }));
      const cls: TimelineBar["cls"] = !done ? "run" : errored ? "err" : clean ? "done" : "canceled";
      const end = !done ? playheadT : closeTs != null ? closeTs : lastTs(data, sid, missionId) || playheadT;
      if (end < tlMin) continue; // ended entirely before the window
      const cst = Math.max(T(s.ts), tlMin); // clip a straddling start to the window edge
      const widthPct = Math.max(0.6, pct(end) - pct(cst));
      const leftPct = Math.max(0, Math.min(pct(cst), 100 - widthPct)); // never spill past the right edge
      const role = (s.handle || "").replace(/^darkmux\//, "");
      const key = missionId ? `${sid}\x1f${missionId}` : sid;
      bars.push({ sid, key, leftPct, widthPct, cls, title: `${role} · ${sid} · ${lbl}` });
    }
    return { uid: m, name: nameOf(data, liveMachines, m), bars };
  });

  return {
    // Legacy appended `· ${clkrange(tlMin,tlMax)}` here (viewer.html:1766);
    // dropped 2026-09-01 — see `headerText`'s own doc. Deliberate divergence
    // from legacy, not drift.
    // (Playback parity, Change A, finding #8) Always "recent activity" now
    // — a replay draws the SAME rolling window as live, anchored at the
    // playhead, so the header no longer needs to say otherwise.
    headerText: "recent activity",
    lanes,
    axis: [clkhm(tlMin), clkhm(tlMin + span / 2), clkhm(tlMax)],
    playheadPct: pct(playheadT),
    labelWidthPx: labelWidthPx(uids, data, liveMachines),
  };
}
