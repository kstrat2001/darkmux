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
 * **The page's clock, not the wall clock.** Live, the page's "now" is the
 * wall clock, read per frame. In playback it is the transport's playhead,
 * which advances at the playback speed on the transport's own 100 ms tick;
 * the scope extrapolates between ticks from the playhead AND the moment the
 * transport computed it (`PageClock`'s `wallMs`), never from when a render
 * happened to land, so render jitter is not a jump in page time.
 *
 * **Monotonic within one rest.** Page time seen by the hand never moves
 * backward within one rest unless the page SEEKS (a scrub, a rewind): a
 * re-anchor that lands a little behind the extrapolation (a pause, a late
 * tick) holds the hand where it was instead of climbing the countdown back
 * (`stepRestHand`).
 *
 * **Effects follow ticks the scope saw.** The tick glow, the finished
 * circle's fade and the number's flare start when the scope observes the
 * number drop between two consecutive frames, never from the phase alone, so
 * a seek or a resume that lands just after a whole second shows none of them.
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

/** Where the page's "now" comes from. */
export type PageClock =
  /** Live: the wall clock (`Date.now()`), read every frame. */
  | { kind: "wall" }
  /** A view that does not move (a finished or parked replay): `tMs`. */
  | { kind: "frozen"; tMs: number }
  /** The playback transport: its playhead `tMs`, the monotonic time
   *  (`performance.now()`) the transport computed it at, and page ms per
   *  wall ms (the speed while playing, 0 while paused). */
  | { kind: "playback"; tMs: number; wallMs: number; rate: number };

export const WALL_CLOCK: PageClock = { kind: "wall" };

/** Page ms per wall ms: 1 live, the speed while playing, 0 otherwise. */
export function pageClockRate(clock: PageClock): number {
  if (clock.kind === "wall") return 1;
  if (clock.kind === "frozen") return 0;
  return Number.isFinite(clock.rate) && clock.rate > 0 ? clock.rate : 0;
}

/** The page's clock at monotonic time `perfMs` (and wall time `dateMs`, for
 *  the live clock). Playback never extrapolates to before its own tick. */
export function pageNowOf(clock: PageClock, perfMs: number, dateMs: number): number {
  if (clock.kind === "wall") return dateMs;
  if (clock.kind === "frozen") return clock.tMs;
  return clock.tMs + Math.max(0, perfMs - clock.wallMs) * pageClockRate(clock);
}

/** Seconds left in the rest (fractional, never negative). */
function restSecondsLeftExact(restEndMs: number, pageNowMs: number): number {
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
function restHandProgress(restEndMs: number, pageNowMs: number): number {
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

/** The geometry of one frame of the hand. */
export interface RestHandGeometry {
  /** The dot's angle (canvas radians). */
  angle: number;
  /** The number the center shows (`ceil` of the seconds left). */
  shown: number;
  /** How much of this second's circle is drawn, in turns (0..1). */
  progress: number;
  /** Draw this second's stroke (the rest is still running). */
  stroke: boolean;
}

/** The hand at page time `pageNowMs`: the dot `progress` of a turn past 12,
 *  drawing this second's stroke while the rest runs. */
export function restHandGeometry(restEndMs: number, pageNowMs: number): RestHandGeometry {
  const shown = restShownSeconds(restEndMs, pageNowMs);
  const progress = restHandProgress(restEndMs, pageNowMs);
  const stroke = restSecondsLeftExact(restEndMs, pageNowMs) > 0;
  return { angle: REST_HAND_TOP + 2 * Math.PI * progress, shown, progress, stroke };
}

/** How long each tick effect lasts at page rate `rate`, in wall ms: its own
 *  duration, or 80% of a page second's wall time when that is shorter (fast
 *  playback), so the ring clears between ticks at any speed. */
export function restEffectMs(baseMs: number, rate: number): number {
  if (!(rate > 0)) return baseMs;
  return Math.min(baseMs, (0.8 * 1000) / rate);
}

/** What the hand carries from one frame to the next. */
export interface RestHandState {
  end: number;
  /** The page's seek generation this state belongs to. */
  seekGen: number;
  /** The latest page time the hand has shown (it never moves backward within
   *  one rest and seek generation). */
  pageMs: number;
  shown: number;
  /** Monotonic wall time of the last tick the scope SAW, or null. */
  tickWallMs: number | null;
  /** How many ticks it has seen in this rest (the flare key). */
  flares: number;
}

/** One frame of the hand. */
export interface RestHandFrame extends RestHandGeometry {
  /** The tick glow at 12 o'clock, 0..1. */
  glow: number;
  /** The finished circle, fading out after an observed tick, 0..1. */
  closedFade: number;
  /** The number's flare duration for the current speed, in wall ms. */
  flareMs: number;
}

export interface RestHandInput {
  end: number;
  /** The page time this frame reads (`pageNowOf`). */
  pageMs: number;
  /** Monotonic wall time of this frame (`performance.now()` timebase). */
  wallMs: number;
  /** Page ms per wall ms. */
  rate: number;
  seekGen: number;
}

/**
 * Advance the hand by one frame.
 *
 * Continuity (the same rest and seek generation as `prev`) makes page time
 * monotonic and lets a drop of the number between the two frames count as a
 * TICK: the tick's wall time is when the page clock crossed that whole
 * second. The glow, the finished circle's fade and the flare run from it.
 * Anything else (the first frame, a new rest, a seek) starts fresh with no
 * tick seen, so nothing fades in from a tick the scope never showed. The
 * final drop to 0 is a tick like any other: the last circle fades and the
 * glow fires.
 */
export function stepRestHand(prev: RestHandState | null, input: RestHandInput): { state: RestHandState; frame: RestHandFrame } {
  const continuous = prev !== null && prev.end === input.end && prev.seekGen === input.seekGen;
  const pageMs = continuous ? Math.max(prev.pageMs, input.pageMs) : input.pageMs;
  const geo = restHandGeometry(input.end, pageMs);
  let tickWallMs = continuous ? prev.tickWallMs : null;
  let flares = continuous ? prev.flares : 0;
  if (continuous && geo.shown < prev.shown) {
    // The page clock crossed `end − shown·1000` somewhere since the last
    // frame; place the tick there in wall time (never after this frame).
    const crossedPage = input.end - geo.shown * 1000;
    const back = input.rate > 0 ? Math.max(0, (pageMs - crossedPage) / input.rate) : 0;
    tickWallMs = input.wallMs - back;
    flares += 1;
  }
  let glow = 0;
  let closedFade = 0;
  if (tickWallMs !== null) {
    const since = Math.max(0, input.wallMs - tickWallMs);
    glow = Math.max(0, 1 - since / restEffectMs(REST_TICK_GLOW_MS, input.rate));
    closedFade = Math.max(0, 1 - since / restEffectMs(REST_CLOSED_FADE_MS, input.rate));
  }
  return {
    state: { end: input.end, seekGen: input.seekGen, pageMs, shown: geo.shown, tickWallMs, flares },
    frame: { ...geo, glow, closedFade, flareMs: restEffectMs(REST_NUMBER_FLARE_MS, input.rate) },
  };
}

/** Reduced motion (prefers-reduced-motion): one still frame, the dot at 12
 *  with no stroke, glow or fade, like every other state's still frame. The
 *  number is the caller's. */
export const REST_STILL_FRAME: RestHandFrame = { angle: REST_HAND_TOP, shown: 0, progress: 0, stroke: false, glow: 0, closedFade: 0, flareMs: 0 };
