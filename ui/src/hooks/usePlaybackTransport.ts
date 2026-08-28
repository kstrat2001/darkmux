import { useCallback, useEffect, useMemo, useState } from "react";
import { T, computeTMax, computeTMin } from "../lib/flow";
import type { FlowRecord } from "../types/handwritten";

/** (#2071) The playback transport, owned by the app shell rather than the
 * playback lens, so play/pause and the scrubber live in the sticky block
 * under the masthead on EVERY route while a recorded day is loaded — the
 * run-detail and mission lenses replay against the same clock, and a phone
 * scrolled 1,500px into the fleet stage still has the controls in view.
 *
 * `dayRecords` is the loaded day (`null` when nothing is loaded: a live
 * daemon route has nothing to scrub). The playhead resets whenever the day
 * itself changes (its span or size), matching the lens's old `[date]`
 * reset. The tick loop advances `t` by a fixed fraction of the day's span
 * per tick so a day of any length plays out in the same wall-clock time. */
export const SPEEDS = [0.5, 1, 2, 4] as const;
export type Speed = (typeof SPEEDS)[number];
export const PLAY_TICK_MS = 100;
export const PLAY_SPAN_DIVISOR = 120;

export interface PlaybackTransport {
  /** A day is loaded and non-empty: the transport renders and the lenses
   * scope to `t`. */
  active: boolean;
  t: number;
  tMin: number;
  tMax: number;
  playing: boolean;
  speed: Speed;
  visibleCount: number;
  totalCount: number;
  scrub: (t: number) => void;
  rewind: () => void;
  togglePlay: () => void;
  cycleSpeed: () => void;
}

export function usePlaybackTransport(dayRecords: FlowRecord[] | null): PlaybackTransport {
  const records = dayRecords && dayRecords.length ? dayRecords : null;
  const tMax = useMemo(() => (records ? computeTMax(records) : 0), [records]);
  const tMin = useMemo(() => (records ? computeTMin(records) : 0), [records]);
  // The day's identity for the reset below: a different file, date, or a
  // re-fetched window changes at least one of these.
  const dayKey = records ? `${tMin}:${tMax}:${records.length}` : "";

  const [t, setT] = useState<number | null>(null);
  const [playing, setPlaying] = useState(false);
  const [speed, setSpeed] = useState<Speed>(1);
  useEffect(() => {
    setT(null);
    setPlaying(false);
    setSpeed(1);
  }, [dayKey]);

  const playheadT = t ?? tMax;

  // Keyed on the day (`dayKey`), not on the records array: a daemon
  // playback route rebuilds that array per render, and an effect keyed on it
  // would clear and restart the interval on every render.
  useEffect(() => {
    if (!playing || !dayKey) return;
    const step = ((tMax - tMin) / PLAY_SPAN_DIVISOR) * speed;
    const id = setInterval(() => {
      setT((prev) => Math.min((prev ?? tMax) + step, tMax));
    }, PLAY_TICK_MS);
    return () => clearInterval(id);
  }, [playing, dayKey, tMin, tMax, speed]);
  useEffect(() => {
    if (playing && playheadT >= tMax) setPlaying(false);
  }, [playing, playheadT, tMax]);

  const scrub = useCallback((next: number) => setT(next), []);
  const rewind = useCallback(() => setT(tMin), [tMin]);
  const togglePlay = useCallback(() => {
    if (playing) {
      setPlaying(false);
      return;
    }
    // Pressing play at the end starts over, same as the lens did. Two plain
    // setters, not a setter inside another's updater (updaters must stay
    // pure; React may invoke them twice).
    if (playheadT >= tMax) setT(tMin);
    setPlaying(true);
  }, [playing, playheadT, tMin, tMax]);
  const cycleSpeed = useCallback(() => setSpeed((s) => SPEEDS[(SPEEDS.indexOf(s) + 1) % SPEEDS.length]), []);

  const visibleCount = useMemo(() => (records ? records.filter((r) => !(T(r.ts) > playheadT)).length : 0), [records, playheadT]);

  return {
    active: records !== null,
    t: playheadT,
    tMin,
    tMax,
    playing,
    speed,
    visibleCount,
    totalCount: records ? records.length : 0,
    scrub,
    rewind,
    togglePlay,
    cycleSpeed,
  };
}
