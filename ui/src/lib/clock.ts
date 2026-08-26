/**
 * (#1972) One shared 1-second clock for the whole app.
 *
 * **Why a store and not a `useState` + `setInterval` per component.** Every
 * component that wanted a ticking value would otherwise own a timer, and they
 * would drift out of phase with each other — two elapsed counters on one page
 * visibly incrementing at different moments. One store means one timer and one
 * `now`, so everything reading it agrees.
 *
 * **The trap this file exists to avoid, documented because it already shipped
 * once.** `useSyncExternalStore` REQUIRES `getSnapshot` to return a
 * referentially stable value when nothing changed. `useHashRoute.ts` returned
 * a fresh object per call and threw React error #185 ("Maximum update depth
 * exceeded") the instant `App` mounted — a real page load never got past a
 * blank screen (see `App.test.tsx`'s header). A clock is the easiest place to
 * repeat it: `getSnapshot = () => Date.now()` returns a DIFFERENT number every
 * call and reproduces the same infinite loop exactly. So the snapshot returns
 * the STORED `nowMs`, which only the interval mutates.
 *
 * **The timer is gated structurally, not by a flag.** The first subscriber
 * starts it and the last one tears it down, so "no ticking component mounted"
 * means "no timer running" by construction. An `enabled` prop can be
 * forgotten; nothing-mounted-means-nothing-ticking cannot.
 */
import { useCallback, useSyncExternalStore } from "react";

/** One second. Anything finer is invisible on a wall-clock readout and
 *  multiplies re-renders for nothing. */
export const CLOCK_INTERVAL_MS = 1000;

let timer: ReturnType<typeof setInterval> | null = null;
let nowMs = Date.now();
const listeners = new Set<() => void>();

function tick(): void {
  nowMs = Date.now();
  for (const l of listeners) l();
}

function subscribeClock(cb: () => void): () => void {
  listeners.add(cb);
  if (timer === null) {
    // Refresh immediately on the FIRST subscriber. The module-level `nowMs`
    // was captured whenever this module was imported, which on a long-lived
    // tab can be hours ago — a first paint showing that would be a visible
    // jump when the first tick corrected it.
    tick();
    timer = setInterval(tick, CLOCK_INTERVAL_MS);
  }
  return () => {
    listeners.delete(cb);
    if (listeners.size === 0 && timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  };
}

/** The STORED value. Never `Date.now()` — see this module's header. */
function getClockSnapshot(): number {
  return nowMs;
}

/** A frozen snapshot for the inactive case. A literal would allocate nothing
 *  either, but naming it makes the stability requirement explicit at the call
 *  site rather than implicit in a `0`. */
const INACTIVE_SNAPSHOT = 0;
function getInactiveSnapshot(): number {
  return INACTIVE_SNAPSHOT;
}
function subscribeNothing(): () => void {
  return () => {};
}

/**
 * `Date.now()`, re-read once a second, but ONLY while `active`.
 *
 * `active` is the run's own liveness: a finished run's elapsed time is a
 * fixed fact and re-rendering it every second is pure waste, while a live
 * run's is the thing the operator is watching. Passing `false` subscribes to
 * nothing and returns a constant, so the hook count stays fixed (a
 * conditional hook would not be legal) while the timer stays off.
 */
export function useNowMs(active: boolean): number {
  const subscribe = useCallback((cb: () => void) => (active ? subscribeClock(cb) : subscribeNothing()), [active]);
  const snapshot = useCallback(() => (active ? getClockSnapshot() : getInactiveSnapshot()), [active]);
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}

/** Test-only: how many components are currently driving the clock, and
 *  whether a timer is actually running. Exported so a test can assert the
 *  STRUCTURAL gate (last unsubscribe stops the timer) rather than trusting
 *  the comment above it. */
export function __clockDebug(): { listeners: number; running: boolean } {
  return { listeners: listeners.size, running: timer !== null };
}
