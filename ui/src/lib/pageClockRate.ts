import { createContext, useContext } from "react";
import type { PageClock } from "./restHand";

/**
 * (#2961) The playback transport's clock, for an animation that follows the
 * page clock between the transport's ticks (REST's seconds hand): the
 * playhead, the monotonic time the transport computed it at, and how fast it
 * runs (recorded ms per wall ms: the speed while playing, 0 while paused).
 * Provided once near the app root from `usePlaybackTransport`, like
 * `SeekSignalContext`; `null` whenever the lenses are not on a playhead (live
 * routes, a replay parked at its end).
 */
export const PlaybackClockContext = createContext<PageClock | null>(null);

export function usePlaybackClock(): PageClock | null {
  return useContext(PlaybackClockContext);
}

/** The context's value, from the transport and the playhead the lenses use.
 *  Paused, the clock stands still (rate 0) even though the speed setting
 *  keeps its value. */
export function playbackClockOf(
  transport: { t: number; tickWallMs: number; playing: boolean; speed: number },
  playhead: number | null,
): PageClock | null {
  if (playhead === null) return null;
  return { kind: "playback", tMs: transport.t, wallMs: transport.tickWallMs, rate: transport.playing ? transport.speed : 0 };
}
