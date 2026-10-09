/**
 * The fleet default view's "recent activity" timeline — `renderFleet()`'s
 * `lanes`/`ax`/`winCtl`/`tl` build. One lane per
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
 * legacy keys on `state.t`: the bar loop's "not started yet" guard, each
 * run's lifecycle, an open bar's `end`, and `playheadPct`. `FleetLens` is the caller that now passes these as two
 * genuinely different numbers on a replay route (see its own doc for the
 * `tMax`/`playhead` prop split this traces back to).
 *
 * The bar loop's "not started yet" guard restores legacy's own
 * `if(!s||T(s.ts)>state.t)return""` — a run that hasn't started yet as of
 * the PLAYHEAD (not the axis ceiling) must not draw a bar at all; its
 * lifecycle reads `not_started` there. See
 * `savings.ts`'s module doc for the parallel restoration applied to the
 * token sums (a caller-side gate, not a change to this file).
 */

import { runStatusWord, type RunBadgeStatus } from "../../lib/runStatusWord";
import { NOT_REPORTING_STATUS } from "../../lib/machineAvailability";
import { displayNameOf } from "../../lib/machineIdentity";
import type { RosterName, SelfIdentity } from "../../lib/machineIdentity";
import { clkhm } from "../../lib/format";
import type { PresenceBeat } from "../../types/generated/PresenceBeat";
import type { NormRecord } from "../../lib/ingest";
import { DEFAULT_POLICY, endMs, lifecycleAt, ownRowOf, shownRunState, spanOf, type LifecyclePolicy, type Presence } from "../../lib/lifecycle";
import type { Run } from "../../types/generated/Run";
import { currentRun, runIndex, type RunGroup } from "../../lib/runRef";
import { dispatchHash } from "../../lib/route";
import { maxOf } from "../../lib/numbers";

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

interface TimelineBar {
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
  /** The bar's click-through: its run's detail view, naming the mission so
   *  a session id several missions share opens this bar's run
   *  (`dispatchHash`). */
  hash: string;
  leftPct: number;
  widthPct: number;
  /** (#2813) The run's status as the board's badge states it (`not_reporting`
   *  for a running run on a machine that is not reporting): the bar's CSS
   *  class, and its color through `workStatusKind`, as the chip's. */
  status: RunBadgeStatus;
  title: string;
}

interface TimelineLane {
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

/** `renderMachine()`'s lane-label width math — sizes
 * the `.lname` column to the longest machine name so short names don't leave
 * a fixed gap. Visual-only (no text-parity effect). */
function labelWidthPx(uids: string[], data: NormRecord[], liveMachines: Map<string, PresenceBeat>, specs: SelfIdentity | null, roster: readonly RosterName[]): number {
  const maxLen = maxOf(uids.map((m) => displayNameOf(data, liveMachines, specs, m, roster).length)) ?? 8;
  return Math.round(Math.min(170, Math.max(54, maxLen * 7.4 + 10)));
}

const NO_IDS: ReadonlySet<string> = new Set();

interface BarWindow {
  tlMin: number;
  pct: (t: number) => number;
  playheadT: number;
  policy: LifecyclePolicy;
  presence: Presence;
  /** Ids (session or mission) of runs the daemon marks `not_reporting`. */
  notReporting: ReadonlySet<string>;
  /** The daemon's `/runs` rows: a bar for a run it lists shows the row's
   *  status (`shownRunState`). */
  rows: readonly Run[];
  /** Whether the playhead is the live edge. */
  live: boolean;
}

/** One run's bar, or `null` when it draws none: bookkeeping-only sessions
 *  (a mission's lifecycle, a scheduler task), a run not started as of the
 *  playhead, and one that ended before the window. The bar spans the run's
 *  first start to where its current attempt ends (`lifecycle.ts`'s `endMs`:
 *  the playhead while it runs, its close, or its last sign of life). */
function barFor(g: RunGroup, w: BarWindow): TimelineBar | null {
  const first = g.attempts[0];
  if (g.grain === "lifecycle" || !first) return null;
  const l = lifecycleAt(currentRun(g, w.playheadT), w.playheadT, w.policy, w.presence);
  if (l.phase === "not_started") return null;
  const state = shownRunState(l, ownRowOf(w.rows, g.sessionId, g.missionId, g.grain), w.live);
  // A run its row says is still running (a lab run verifying after its
  // dispatch ended) reaches the playhead, as any running bar does.
  const end = state.status === "running" ? w.playheadT : (endMs(l, w.playheadT) ?? w.playheadT);
  if (end < w.tlMin) return null;
  // Clip a straddling start to the window edge; an untimed start draws from
  // the edge, visible rather than dropped.
  const cst = Math.max(spanOf(g).startMs ?? w.tlMin, w.tlMin);
  const widthPct = Math.max(0.6, w.pct(end) - w.pct(cst));
  const leftPct = Math.max(0, Math.min(w.pct(cst), 100 - widthPct)); // never spill past the right edge
  const role = ((first.start ?? first.opening).handle || "").replace(/^darkmux\//, "");
  const silent = state.status === "running" && (w.notReporting.has(g.sessionId) || (g.missionId !== null && w.notReporting.has(g.missionId)));
  const status: RunBadgeStatus = silent ? NOT_REPORTING_STATUS : state.status;
  const word = runStatusWord(status, state.abandonReason);
  const key = g.missionId ? `${g.sessionId}\x1f${g.missionId}` : g.sessionId;
  return { sid: g.sessionId, key, hash: dispatchHash(g.sessionId, g.missionId), leftPct, widthPct, status, title: `${role} · ${g.sessionId} · ${word}` };
}

export function buildActivityTimeline(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  uids: string[],
  presence: Presence,
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
  /** (#2921) This daemon's own identity, so the lane label is the same
   *  `displayNameOf` title the machine's card carries. */
  specs: SelfIdentity | null = null,
  /** (#2921 follow-up) The declared roster, for the same reason. */
  roster: readonly RosterName[] = [],
  /** The daemon's lifecycle policy (`/runs.policy`). */
  policy: LifecyclePolicy = DEFAULT_POLICY,
  /** Ids of runs the daemon marks `not_reporting` (`Run.not_reporting`): their
   *  bar titles say so, as the board does. */
  notReporting: ReadonlySet<string> = NO_IDS,
  /** The daemon's `/runs` rows: a bar for a run it lists shows the row's
   *  status, decided once (`shownRunState`). */
  rows: readonly Run[] = [],
  /** Whether `playheadT` is the live edge (no parked playhead). */
  live = false,
): ActivityTimeline {
  const winMs = windowMinutes * 60000;
  const tlMax = fixedRange ? fixedRange[1] : playheadT;
  const tlMin = fixedRange ? fixedRange[0] : tlMax - winMs;
  const span = Math.max(1, tlMax - tlMin);
  const pct = (t: number) => ((t - tlMin) / span) * 100;

  const window: BarWindow = { tlMin, pct, playheadT, policy, presence, notReporting, rows, live };
  const lanes: TimelineLane[] = uids.map((m) => {
    // (#2125) One bar per RUN, a `(session, mission)` pair (`runRef.ts`),
    // not per bare session id: a review mission's step session id is reused
    // by every review run, and pairing one mission's start with another's
    // end drew a 20-hour abandoned span for a 23-minute mission.
    const bars: TimelineBar[] = [];
    for (const g of runIndex(data).groupsOn(m)) {
      const bar = barFor(g, window);
      if (bar) bars.push(bar);
    }
    return { uid: m, name: displayNameOf(data, liveMachines, specs, m, roster), bars };
  });

  return {
    // Legacy appended `· ${clkrange(tlMin,tlMax)}` here;
    // dropped 2026-09-01 — see `headerText`'s own doc. Deliberate divergence
    // from legacy, not drift.
    // (Playback parity, Change A, finding #8) Always "recent activity" now
    // — a replay draws the SAME rolling window as live, anchored at the
    // playhead, so the header no longer needs to say otherwise.
    headerText: "recent activity",
    lanes,
    axis: [clkhm(tlMin), clkhm(tlMin + span / 2), clkhm(tlMax)],
    playheadPct: pct(playheadT),
    labelWidthPx: labelWidthPx(uids, data, liveMachines, specs, roster),
  };
}
