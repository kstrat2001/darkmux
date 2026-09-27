/**
 * (#2961) REST's seconds hand: the dot on the breathing ring that loops once
 * per second and reaches 12 o'clock exactly when the countdown in the tube's
 * center drops a number.
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

/** The trail's length, in turns (operator, 2026-09-27: longer than the
 *  prototype's 0.45). */
export const REST_TRAIL_TURNS = 0.7;

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

/** The fraction of the current second the hand has swept, 0 at the tick (the
 *  moment the number drops) and approaching 1 just before the next. 0 once
 *  the rest has ended. */
export function restHandProgress(restEndMs: number, pageNowMs: number): number {
  const s = restSecondsLeftExact(restEndMs, pageNowMs);
  if (s <= 0) return 0;
  const frac = s - Math.floor(s);
  return frac === 0 ? 0 : 1 - frac;
}

/** The hand's angle: the top plus `1 − frac(seconds left)` of a turn,
 *  normalized to `[top, top + 2π)`. At a tick it is exactly the top. */
export function restHandAngle(restEndMs: number, pageNowMs: number): number {
  return REST_HAND_TOP + 2 * Math.PI * restHandProgress(restEndMs, pageNowMs);
}

/** What one frame of the hand draws. */
export interface RestHandFrame {
  /** The hand's angle (canvas radians). */
  angle: number;
  /** The number the center shows (`ceil` of the seconds left). */
  shown: number;
  /** Draw the trail (the rest is running and motion is allowed). */
  trail: boolean;
  /** The tick glow at 12 o'clock, 0..1 (0 = none). */
  glow: number;
}

/**
 * One frame of the hand at page time `pageNowMs`.
 *
 * `rate` (page ms per wall ms) converts the page time since the last tick to
 * wall time for the glow, whose ~180 ms is a visual duration. While the page
 * clock stands still (`rate` 0: paused, or a frozen view) nothing moves, so
 * there is no glow. `reduced` (prefers-reduced-motion): one still frame, the
 * hand at the top, no trail or glow; the number still counts.
 */
export function restHandFrame(restEndMs: number, pageNowMs: number, rate: number, reduced: boolean): RestHandFrame {
  const shown = restShownSeconds(restEndMs, pageNowMs);
  if (reduced) return { angle: REST_HAND_TOP, shown, trail: false, glow: 0 };
  const running = restSecondsLeftExact(restEndMs, pageNowMs) > 0;
  const progress = restHandProgress(restEndMs, pageNowMs);
  let glow = 0;
  if (running && rate > 0) {
    const wallSinceTick = (progress * 1000) / rate;
    glow = Math.max(0, 1 - wallSinceTick / REST_TICK_GLOW_MS);
  }
  return { angle: REST_HAND_TOP + 2 * Math.PI * progress, shown, trail: running, glow };
}
