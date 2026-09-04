import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { T, computeTMax, computeTMin, isDispatchStart, isDispatchTerminal } from "../lib/flow";
import type { FlowRecord } from "../types/handwritten";

/** (#2346) What the transport's `tMin`/`tMax` scope to. `day` (the
 * default) is the pre-existing behavior — the whole loaded day. A
 * `dispatch`/`mission` focus narrows the range to that ONE thing's own
 * span, so the playhead's ceiling is the run's/mission's own end rather
 * than the day's last record — the bug this exists to fix (#2346): the
 * masthead scrubber pinned at the day's own last record while the open
 * run's own wall clock ended hours earlier, two clocks describing two
 * different subjects.
 *
 * `records` carries the focus's OWN fetch (`routeRecords.records` for a
 * dispatch, `missionRecordsQuery.data` for a mission — `App.tsx`'s own
 * doc) — NEVER derived by filtering `dayRecords`. A live daemon's
 * `/flow/<date>` is a CAPPED, TIME-WINDOWED slice (#2346 live-render
 * finding): a long-running dispatch that started hours before the window's
 * current floor has ZERO records inside it, so filtering `dayRecords` by
 * `session_id` silently found nothing and fell back to the whole-day range
 * — reproducing the exact bug this hook exists to fix. The focus's own
 * per-session/per-mission fetch has no such cap. */
export type PlaybackFocus =
  | { kind: "day" }
  | { kind: "dispatch"; sessionId: string; records: FlowRecord[] }
  | { kind: "mission"; missionId: string; records: FlowRecord[] };

const DAY_FOCUS: PlaybackFocus = { kind: "day" };

/** A stable primitive identity for a focus — used as a `useMemo`/effect key
 * instead of the focus OBJECT's own identity, which a caller (`App.tsx`)
 * has no reason to keep stable across renders (a fresh `{kind:"dispatch",
 * sessionId, records}` literal every render is normal React, not a bug). */
function focusKeyOf(focus: PlaybackFocus): string {
  if (focus.kind === "dispatch") return `dispatch:${focus.sessionId}`;
  if (focus.kind === "mission") return `mission:${focus.missionId}`;
  return "day";
}

/** A cheap signature for the focus's OWN records — length plus the last
 * one's `ts` — used as a THIRD memo key alongside `focusKeyOf` above.
 * `routeRecords.records` is rebuilt (a new array, same content) on every
 * render of its own producer (`useRouteRecords`'s `recordsOf` is not
 * memoized), so keying on the array's REFERENCE would recompute the range
 * on every tick of the transport's own 100ms play loop — the exact waste
 * `focusKeyOf` above already avoids for the id. Length+last-ts is not a
 * cryptographic guarantee, but a session's/mission's own record set only
 * ever APPENDS while it is open, so this changes exactly when the content
 * that matters (the newest bookend) changes, and settles once it does. */
function focusRecordsSignature(focus: PlaybackFocus): string {
  if (focus.kind === "day") return "";
  const recs = focus.records;
  return recs.length ? `${recs.length}:${recs[recs.length - 1].ts}` : "0";
}

/** The focus's own span. `dispatch` prefers the session's `dispatch.start`/
 * terminal (`dispatch.complete`/`dispatch.error`) records — the same
 * bookend matchers `lib/flow.ts` already exports for this exact
 * "which spelling did this producer use" question (#1852) — and falls
 * back to the session's earliest/latest record when a bookend is missing
 * (a session mid-flight has no terminal yet; an oddly-shaped record set
 * has no bookend at all). `mission` mirrors it with the mission lifecycle
 * actions (`missionReplayDate`'s own vocabulary, `lib/flow.ts`).
 *
 * `dayRecords` is used ONLY as the fallback range — never to find the
 * focus's own records (see the `PlaybackFocus` doc above for why that was
 * the bug). Falls back to the WHOLE day when the focus's own fetch hasn't
 * landed yet (still loading: `focus.records` is empty) or has nothing
 * matching (a stale/foreign id) — an empty range is worse than a
 * wrong-but-honest one, and the moment the real records arrive this
 * function starts returning the narrow range instead (the preserve-or-snap
 * effect in the hook below is what makes the PLAYHEAD follow that change,
 * not just the range). */
function focusRange(dayRecords: FlowRecord[], focus: PlaybackFocus): { tMin: number; tMax: number } {
  if (focus.kind === "day") return { tMin: computeTMin(dayRecords), tMax: computeTMax(dayRecords) };

  const scoped =
    focus.kind === "dispatch"
      ? focus.records.filter((r) => r.session_id === focus.sessionId)
      : focus.records.filter((r) => r.mission_id === focus.missionId);
  if (!scoped.length) return { tMin: computeTMin(dayRecords), tMax: computeTMax(dayRecords) };

  const startMatch = (r: FlowRecord): boolean =>
    focus.kind === "dispatch" ? isDispatchStart(r.action) : r.action === "mission start";
  const endMatch = (r: FlowRecord): boolean =>
    focus.kind === "dispatch" ? isDispatchTerminal(r.action) : r.action === "mission close" || r.action === "mission abort";

  const starts = scoped.filter(startMatch).map((r) => T(r.ts)).filter((n) => !Number.isNaN(n));
  const ends = scoped.filter(endMatch).map((r) => T(r.ts)).filter((n) => !Number.isNaN(n));
  return {
    tMin: starts.length ? Math.min(...starts) : computeTMin(scoped),
    tMax: ends.length ? Math.max(...ends) : computeTMax(scoped),
  };
}

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

export function usePlaybackTransport(dayRecords: FlowRecord[] | null, focus: PlaybackFocus = DAY_FOCUS): PlaybackTransport {
  const records = dayRecords && dayRecords.length ? dayRecords : null;
  const focusKey = focusKeyOf(focus);
  const focusRecordsSig = focusRecordsSignature(focus);
  // The day's identity for the reset below, computed over the WHOLE day —
  // deliberately independent of `focus`: a different file, date, or a
  // re-fetched window changes at least one of these, and that is what
  // warrants starting the transport over from scratch. A focus change alone
  // (opening a different dispatch on the SAME day, or that dispatch's own
  // records finally arriving) is handled by the separate preserve-or-snap
  // effect further down, not this one.
  const dayIdentity = records ? `${computeTMin(records)}:${computeTMax(records)}:${records.length}` : "";
  const range = useMemo(
    () => (records ? focusRange(records, focus) : { tMin: 0, tMax: 0 }),
    // Keyed on `focusKey` + `focusRecordsSig` (both primitives), not `focus`
    // (an object literal a caller has no reason to keep referentially
    // stable across renders, and whose OWN `.records` array is rebuilt
    // every render by its producer — `App.tsx`'s doc on `dispatchFocusRecords`)
    // — recomputing this on every render would re-derive the range on every
    // 100ms playback tick, which only ever changes `t`, never the range.
    // `focusRecordsSig` is what makes this recompute the moment the focus's
    // own fetch lands (#2346): the id doesn't change when a session's
    // records go from "still loading" to "here", but the signature does.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [records, focusKey, focusRecordsSig],
  );
  const tMax = range.tMax;
  const tMin = range.tMin;

  const [t, setT] = useState<number | null>(null);
  const [playing, setPlaying] = useState(false);
  const [speed, setSpeed] = useState<Speed>(DEFAULT_SPEED);
  useEffect(() => {
    setT(null);
    setPlaying(false);
    setSpeed(DEFAULT_SPEED);
  }, [dayIdentity]);

  const playheadT = t ?? tMax;

  // (#2346) A RANGE change on the SAME day preserves the ABSOLUTE playhead
  // when it still falls inside the new range, and otherwise snaps to the
  // new range's own end — the same "pinned at end until scrubbed" default a
  // fresh focus gets. Two distinct events both count as a range change, and
  // this effect must catch BOTH:
  //
  //   1. A focus SWITCH (opening a different dispatch/mission, or leaving
  //      one for the day view) — `focusKey` changes.
  //   2. The CURRENT focus's own records arriving LATE (mount fetches the
  //      session; `focus.records` starts empty, so `focusRange` falls back
  //      to the day's range; the fetch lands and the range narrows to the
  //      run's own span) — `focusKey` is UNCHANGED across this, only
  //      `tMin`/`tMax` move. Keying this effect on `[focusKey, tMin, tMax]`
  //      (not `[focusKey]` alone) is what makes case 2 land the same snap
  //      case 1 gets — the live-render finding this redesign fixes: the day
  //      window was capped and time-windowed, so the run's own records were
  //      never IN `dayRecords`, and the transport had to wait on the
  //      dispatch's own fetch before it could compute the real range at
  //      all.
  //
  // `prevRef` carries the PRIOR render's resolved values (written by the
  // unconditional effect below, which always runs after this one within a
  // commit) so this effect compares against the playhead as it stood the
  // instant before the range changed, not the value this same render
  // already recomputed against the NEW tMax.
  //
  // Guarded on `prev.dayIdentity === dayIdentity`: when the day ALSO
  // changed in this render, the reset effect above owns it — this one
  // no-ops rather than fighting it over which `setT` wins.
  const prevRef = useRef<{ dayIdentity: string; playheadT: number } | null>(null);
  useEffect(() => {
    const prev = prevRef.current;
    if (prev && prev.dayIdentity === dayIdentity) {
      if (prev.playheadT >= tMin && prev.playheadT <= tMax) {
        setT(prev.playheadT >= tMax ? null : prev.playheadT);
      } else {
        setT(null);
      }
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusKey, tMin, tMax]);
  useEffect(() => {
    prevRef.current = { dayIdentity, playheadT };
  });

  // Keyed on the day (`dayIdentity`), not on the records array: a daemon
  // playback route rebuilds that array per render, and an effect keyed on it
  // would clear and restart the interval on every render. The step is
  // recorded time per tick, so a day of any length plays at the labeled
  // rate rather than in a fixed number of ticks.
  // `lastTick` survives an effect restart (a speed change re-arms the
  // interval), so the partial tick in flight at that moment is not lost —
  // it lands on the next tick at the new speed.
  const lastTick = useRef<number | null>(null);
  useEffect(() => {
    if (!playing || !dayIdentity) {
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
  }, [playing, dayIdentity, tMin, tMax, speed]);
  useEffect(() => {
    if (playing && playheadT >= tMax) setPlaying(false);
  }, [playing, playheadT, tMax]);

  // (#2346) Clamped to the focus's own range: the range input's own drag
  // math already stays in bounds (0..100% of `[tMin, tMax]`), but `scrub`
  // is a public part of the transport's API — a direct call (a test, a
  // future caller) must not be able to punch the playhead outside the
  // focus it belongs to.
  const scrub = useCallback((next: number) => setT(Math.min(tMax, Math.max(tMin, next))), [tMin, tMax]);
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
