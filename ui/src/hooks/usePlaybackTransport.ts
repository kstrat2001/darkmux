import { useCallback, useEffect, useMemo, useRef, useState } from "react";
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
 * reset. The tick loop advances `t` by the MEASURED elapsed wall clock
 * since the previous tick times the speed, so the labeled rate holds on a
 * day of any length, and holds when the browser throttles the interval
 * (a background tab runs `setInterval` once a second; a nominal step would
 * then replay at a tenth of the label). */
/** Speed is a REAL multiplier of elapsed time: at `3600` one second of
 * wall clock replays one recorded hour. It used to be a fraction of the
 * day per tick (`PLAY_SPAN_DIVISOR = 120`: any recording, however long,
 * played out in 12 seconds), which made the "1×" label a lie — a 13-hour
 * demo day ran at ~3,900× real time under it (operator: "1× doesn't seem
 * 1×"). Labeled as recorded time per second (`speedLabel`) because a bare
 * multiplier is meaningless to a reader: `1h/s` is. Cycle order steps
 * DOWN from the default (1h/s → 10m/s → 1m/s) and wraps. */
export const SPEEDS = [3600, 600, 60] as const;
export type Speed = (typeof SPEEDS)[number];
export const DEFAULT_SPEED: Speed = 3600;
export const PLAY_TICK_MS = 100;

export function speedLabel(speed: number): string {
  if (speed % 3600 === 0) return `${speed / 3600}h/s`;
  if (speed % 60 === 0) return `${speed / 60}m/s`;
  return `${speed}s/s`;
}

export interface PlaybackTransport {
  /** A day is loaded and non-empty: the transport renders and the lenses
   * scope to `t`. */
  active: boolean;
  /** The playhead has been moved (scrub, rewind, play) since the day
   * loaded. Until then `t` is pinned at the day's end and NOTHING is cut:
   * a session or mission that ran past the loaded day's last record (a
   * run crossing midnight) must render whole by default, exactly as it
   * did before the transport existed. Lenses scope to `t` only while this
   * is true. */
  scrubbed: boolean;
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
  const [speed, setSpeed] = useState<Speed>(DEFAULT_SPEED);
  useEffect(() => {
    setT(null);
    setPlaying(false);
    setSpeed(DEFAULT_SPEED);
  }, [dayKey]);

  const playheadT = t ?? tMax;

  // Keyed on the day (`dayKey`), not on the records array: a daemon
  // playback route rebuilds that array per render, and an effect keyed on it
  // would clear and restart the interval on every render. The step is
  // recorded time per tick, so a day of any length plays at the labeled
  // rate rather than in a fixed number of ticks.
  // `lastTick` survives an effect restart (a speed change re-arms the
  // interval), so the partial tick in flight at that moment is not lost —
  // it lands on the next tick at the new speed.
  const lastTick = useRef<number | null>(null);
  useEffect(() => {
    if (!playing || !dayKey) {
      lastTick.current = null;
      return;
    }
    if (lastTick.current === null) lastTick.current = performance.now();
    const id = setInterval(() => {
      const now = performance.now();
      const dt = now - (lastTick.current ?? now);
      lastTick.current = now;
      const step = dt * speed; // recorded ms this tick
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
    scrubbed: t !== null,
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
