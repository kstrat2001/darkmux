/**
 * (#2961) REST's seconds hand, design B (operator, 2026-09-27): while a REST
 * countdown runs, the breathing ring is gone and the dot DRAWS the circle
 * over each second, from 12 o'clock clockwise to itself, dimmer along its
 * length by how long ago each part was drawn. It reaches 12 exactly when the
 * countdown in the tube's center drops a number; there the finished circle
 * fades out quickly while the next one starts.
 *
 * Everything here is a pure function of the rest's END time and the page's
 * clock, so the hand and the number cannot drift apart: the angle is
 * `1 − frac(seconds left)` of a turn from the top, and the number is
 * `ceil(seconds left)` (the same `Math.ceil` `deriveLiveState` counts
 * with). Both change at the same instant, when the seconds left cross a whole
 * number, and at that instant the angle is the top.
 *
 * **The page's clock, not the wall clock.** In playback the page's "now" is
 * the transport's playhead, which advances at the playback speed. The scope
 * redraws every animation frame, while the playhead only moves on the
 * transport's own tick (100 ms) and a live page re-renders once a second, so
 * the scope extrapolates the page clock between updates from an anchor: the
 * page time an update was taken at, the wall time (a monotonic
 * `performance.now()`) it arrived, and how many page ms pass per wall ms (1
 * live, the playback speed while playing, 0 when paused or frozen).
 */

/** Where 12 o'clock is, in canvas radians (0 is 3 o'clock, y points down). */
export const REST_HAND_TOP = -Math.PI / 2;

/** How bright the stroke is at 12 when the circle closes (a segment drawn a
 *  full second ago), relative to the newest segment at the dot. */
export const REST_STROKE_FLOOR = 0.25;

/** How long the finished circle takes to fade out after a tick, in wall ms. */
export const REST_CLOSED_FADE_MS = 220;

/** The tick glow at 12 o'clock, in wall ms. */
export const REST_TICK_GLOW_MS = 180;

/** The number's flare on each tick, in wall ms (matches
 *  `token-scope-rest-tick` in `styles.css`). */
export const REST_NUMBER_FLARE_MS = 260;

/** The page's clock, anchored where an update arrived. */
export interface PageClockAnchor {
  /** The page's "now" (epoch ms) the update was derived at. */
  pageMs: number;
  /** The monotonic wall time (`performance.now()`) the anchor was taken. */
  wallMs: number;
  /** Page ms per wall ms: 1 live, the playback speed while playing, 0 when
   *  paused or frozen. */
  rate: number;
}

/** The page's clock at monotonic wall time `wallMs`. Never runs backward
 *  from its anchor (a wall time before the anchor reads as the anchor). */
export function pageNowAt(anchor: PageClockAnchor, wallMs: number): number {
  const rate = Number.isFinite(anchor.rate) && anchor.rate > 0 ? anchor.rate : 0;
  return anchor.pageMs + Math.max(0, wallMs - anchor.wallMs) * rate;
}

/** Seconds left in the rest (fractional, never negative). */
export function restSecondsLeftExact(restEndMs: number, pageNowMs: number): number {
  return Math.max(0, (restEndMs - pageNowMs) / 1000);
}

/** The whole seconds the countdown shows: `Math.ceil`, like
 *  `deriveLiveState`'s `restSecondsLeft`. 0 once the rest has ended. */
export function restShownSeconds(restEndMs: number, pageNowMs: number): number {
  return Math.ceil(restSecondsLeftExact(restEndMs, pageNowMs));
}

/** The fraction of the current second the dot has drawn, `1 − frac(seconds
 *  left)`: 0 at the tick (the moment the number drops), approaching 1 just
 *  before the next. 0 once the rest has ended. */
export function restHandProgress(restEndMs: number, pageNowMs: number): number {
  const s = restSecondsLeftExact(restEndMs, pageNowMs);
  if (s <= 0) return 0;
  const frac = s - Math.floor(s);
  return frac === 0 ? 0 : 1 - frac;
}

/** The dot's angle: the top plus `progress` of a turn, in `[top, top + 2π)`.
 *  At a tick it is exactly the top. */
export function restHandAngle(restEndMs: number, pageNowMs: number): number {
  return REST_HAND_TOP + 2 * Math.PI * restHandProgress(restEndMs, pageNowMs);
}

/** The stroke's brightness for a part drawn `ageTurns` ago (0 = just drawn
 *  at the dot, 1 = a full second ago, at 12 when the circle closes):
 *  `1 − (1 − floor)·age`, so 1.0 at the dot and 0.25 at a closed circle's
 *  start. Clamped to that range. */
export function restStrokeBrightness(ageTurns: number): number {
  const age = Math.min(1, Math.max(0, ageTurns));
  return 1 - (1 - REST_STROKE_FLOOR) * age;
}

/** One unblurred segment of the stroke, in turns from 12 o'clock. */
export interface RestStrokeSegment {
  from: number;
  to: number;
  brightness: number;
}

/** The stroke from 12 o'clock to `progress` of a turn, cut into segments
 *  (about 96 per full turn), each as bright as its midpoint's age. The
 *  segments cover `[0, progress]` exactly. */
export function restStrokeSegments(progress: number): RestStrokeSegment[] {
  if (!(progress > 0)) return [];
  const p = Math.min(1, progress);
  const n = Math.max(2, Math.ceil(p * 96));
  const out: RestStrokeSegment[] = [];
  for (let i = 0; i < n; i++) {
    const from = (i / n) * p;
    const to = ((i + 1) / n) * p;
    out.push({ from, to, brightness: restStrokeBrightness(p - (from + to) / 2) });
  }
  return out;
}

/** What one frame of the hand draws. */
export interface RestHandFrame {
  /** The dot's angle (canvas radians). */
  angle: number;
  /** The number the center shows (`ceil` of the seconds left). */
  shown: number;
  /** How much of this second's circle is drawn, in turns (0..1). */
  progress: number;
  /** Draw this second's stroke (the rest is running and motion is allowed). */
  stroke: boolean;
  /** The tick glow at 12 o'clock, 0..1 (0 = none). */
  glow: number;
  /** The finished circle (the previous second's), fading out after the
   *  tick: 1 at the tick, 0 after `REST_CLOSED_FADE_MS` of wall time. */
  closedFade: number;
}

/**
 * One frame of the hand at page time `pageNowMs`.
 *
 * `rate` (page ms per wall ms) converts the page time since the last tick to
 * wall time for the glow and the finished circle's fade, which are visual
 * durations. While the page clock stands still (`rate` 0: paused, or a
 * frozen view) nothing fades, so neither shows. `reduced`
 * (prefers-reduced-motion): one still frame, the dot at 12 with no stroke,
 * glow or fade, like every other state's still frame; the number still
 * counts.
 */
export function restHandFrame(restEndMs: number, pageNowMs: number, rate: number, reduced: boolean): RestHandFrame {
  const shown = restShownSeconds(restEndMs, pageNowMs);
  if (reduced) return { angle: REST_HAND_TOP, shown, progress: 0, stroke: false, glow: 0, closedFade: 0 };
  const running = restSecondsLeftExact(restEndMs, pageNowMs) > 0;
  const progress = restHandProgress(restEndMs, pageNowMs);
  let glow = 0;
  let closedFade = 0;
  if (running && rate > 0) {
    const wallSinceTick = (progress * 1000) / rate;
    glow = Math.max(0, 1 - wallSinceTick / REST_TICK_GLOW_MS);
    closedFade = Math.max(0, 1 - wallSinceTick / REST_CLOSED_FADE_MS);
  }
  return { angle: REST_HAND_TOP + 2 * Math.PI * progress, shown, progress, stroke: running, glow, closedFade };
}
