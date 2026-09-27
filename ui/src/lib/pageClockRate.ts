import { createContext, useContext } from "react";

/**
 * (#2961) How fast the page's clock runs while the playback transport drives
 * it: recorded ms per wall ms. The transport's `speed` while it plays, 0 while
 * it is paused or parked at a scrubbed instant. Provided once near the app
 * root from `usePlaybackTransport`, like `SeekSignalContext`.
 *
 * A lens whose clock is the playhead reads this to tell an animation that
 * follows the page clock between renders (REST's seconds hand, which must
 * reach 12 o'clock when the countdown drops) how far the clock moves per
 * wall ms. A lens on the live wall clock uses 1 and never reads it. The
 * default (0) is a clock that stands still, the safe reading when nothing
 * drives it.
 */
export const PageClockRateContext = createContext<number>(0);

export function usePlaybackClockRate(): number {
  return useContext(PageClockRateContext);
}
