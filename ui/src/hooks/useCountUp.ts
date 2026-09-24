import { useEffect, useRef, useState } from "react";
import { usePrefersReducedMotion } from "./usePrefersReducedMotion";

const DEFAULT_DURATION_MS = 700;

/** Ease-out cubic — a fast start that settles into the new value rather
 * than a linear count, matching the gauge/battery CSS transitions'
 * `ease-out` timing so a tile's number and a dial's fill feel like the
 * same instrument. */
function easeOutCubic(t: number): number {
  return 1 - Math.pow(1 - t, 3);
}

/**
 * (#2878) "Numbers count to their new value" — a small shared count-up
 * tween for any figure that changes on a live page: a gauge's caption
 * percentage, the battery percentage, the fleet hero's token total, a
 * run's MODEL/SYSTEM metric tiles.
 *
 * Deliberately generic over `number | null` rather than `number` alone —
 * this app's own "absence is a different claim than zero" rule
 * (`Meter.tsx`'s own doc) means a reading can legitimately be unmeasured,
 * and animating FROM 0 the first time a real number arrives would assert
 * a `0` reading that was never taken. So:
 *
 * - **First render ever**: adopts `target` immediately, no tween — an
 *   already-rendered page must never show a number climbing from 0 on
 *   load.
 * - **`null` on either side of a change** (a reading appears, or is lost):
 *   snaps to the new state immediately. There is no meaningful
 *   in-between value to interpolate through absence.
 * - **A real change between two known numbers**: tweens over `durationMs`
 *   with `requestAnimationFrame`, ease-out, and stops raf'ing the moment
 *   it lands — no perpetual loop while the page is otherwise idle.
 * - **`prefers-reduced-motion: reduce`**: every change lands instantly,
 *   same as the CSS-driven motion elsewhere on this page.
 *
 * `format` is the caller's OWN existing formatter (`fmtPct`, `fmtN`, …) —
 * this hook only ever hands it a number (or `null`), never touches digit
 * grouping, rounding or units itself, so a number's printed shape is
 * exactly what it always was.
 *
 * - **`upOnly`**: only an increase tweens; a decrease lands instantly. For a
 *   figure that falls by bookkeeping rather than by activity, e.g. the fleet
 *   hero's "last 24h" total, which shrinks on every poll as the window slides
 *   past old records. Tweening that made an idle fleet look busy.
 * - **A change mid-tween** continues from the number currently on screen,
 *   not from the previous target, so the figure never jumps.
 */
export function useCountUp(
  target: number | null,
  format: (n: number | null) => string,
  durationMs: number = DEFAULT_DURATION_MS,
  opts: { upOnly?: boolean } = {},
): string {
  const prevTarget = useRef<number | null | typeof UNSET>(UNSET);
  const [display, setDisplay] = useState<number | null>(target);
  const shown = useRef<number | null>(target);
  shown.current = display;
  const upOnly = opts.upOnly === true;
  const rafRef = useRef<number | null>(null);
  const reduced = usePrefersReducedMotion();

  useEffect(() => {
    if (prevTarget.current === UNSET) {
      // First mount: render the true value immediately (never animate on
      // first paint — see this hook's own doc).
      prevTarget.current = target;
      setDisplay(target);
      return;
    }
    const from = prevTarget.current;
    prevTarget.current = target;
    if (from === target) return;
    // `durationMs <= 0` is a caller opt-out — a scrubbed playhead or a
    // replay jump is a discrete seek to a different already-happened
    // instant, not a live value changing over time, and animating BETWEEN
    // two unrelated instants would show a number that was never true at
    // either point in time (the fleet hero's own `liveMode` gate is the
    // first caller of this — see `FleetLens.tsx`).
    if (from === null || target === null || reduced || durationMs <= 0 || (upOnly && target < from)) {
      setDisplay(target);
      return;
    }
    // From what is on screen: a tween interrupted by a new target continues
    // from where it was, rather than restarting at the previous target.
    const startVal = shown.current ?? from;
    const endVal = target;
    const start = performance.now();
    const tick = (now: number) => {
      const t = Math.min(1, (now - start) / durationMs);
      setDisplay(startVal + (endVal - startVal) * easeOutCubic(t));
      rafRef.current = t < 1 ? requestAnimationFrame(tick) : null;
    };
    rafRef.current = requestAnimationFrame(tick);
    return () => {
      if (rafRef.current !== null) cancelAnimationFrame(rafRef.current);
      rafRef.current = null;
    };
  }, [target, reduced, durationMs, upOnly]);

  return format(display);
}

// A dedicated sentinel rather than `undefined` — `target` is already
// `number | null`, and a caller passing `undefined` by mistake (a type
// error, but JS won't stop it) must not be silently read as "first mount".
const UNSET = Symbol("useCountUp:unset");
