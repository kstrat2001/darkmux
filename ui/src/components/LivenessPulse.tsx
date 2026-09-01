/**
 * (#1972) A dot that pulses while a run is demonstrably alive, and holds
 * still when it goes quiet.
 *
 * **The element is never remounted, and that is the whole design.** A pulse
 * implemented by re-rendering on each beat restarts its CSS animation every
 * time, so the dot stutters at whatever irregular cadence the records happen
 * to arrive at — which reads as a fault rather than as health. Here beats only
 * move a NUMBER (`lastBeatMs`); the animation runs continuously and is paused
 * via `animation-play-state` when the run has gone quiet. Nothing restarts.
 *
 * **Why 5 seconds.** `HEARTBEAT_MIN_INTERVAL` in `dispatch_internal` is 2s, so
 * a healthy run proves itself at least that often. 5s is 2.5x that margin: one
 * dropped or delayed beat cannot make a working run look stalled, while a
 * genuinely stuck dispatch stops pulsing within a few seconds. Tying the
 * threshold to the emitter's own constant is the point — a number picked
 * independently would drift the first time that cadence changed.
 */

/** How long without proof of life before the pulse holds still.
 *
 *  **This must exceed the CLIENT's poll interval, not just the emitter's
 *  heartbeat cadence.** The pulse measures how fresh the newest RECORD is,
 *  and the client cannot learn about a beat until it next polls — so record
 *  freshness is bounded below by `PRESENCE_POLL_MS` (5s) no matter how
 *  briskly the run emits. At the original 5000 the two were equal, and a
 *  perfectly healthy run reported quiet for roughly half of every poll cycle:
 *  the pulse was reporting the CLIENT's polling gap as the RUN's silence.
 *
 *  12s = poll (5s) + heartbeat (2s) + margin. A genuinely stuck dispatch
 *  still stops pulsing well inside the watchdog's own inactivity budget. */
export const PULSE_QUIET_AFTER_MS = 12_000;

export interface LivenessPulseProps {
  /** Has the run reached a terminal record? This is the SEMANTIC question —
   *  what the pill says — and it is deliberately separate from `animate`. */
  done: boolean;
  /** Is the run plausibly executing right now (open AND recently active)?
   *  Drives the animation and the clock subscription only.
   *
   *  These were ONE prop, and the two questions disagree for a real case: a
   *  run that opened and went silent for a month has no terminal record, so
   *  the pill says RUNNING, while liveness says it cannot still be executing.
   *  Passing one boolean for both made the pill read `RUNNING` while this
   *  element announced `finished` — the same run, the same view, opposite
   *  claims, with only screen-reader users seeing the contradiction. */
  animate: boolean;
  /** When the newest record for this run landed, or `null` if none has. */
  lastBeatMs: number | null;
}

/** (operator, 2026-09-01) The dot was removed and the RUNNING pill pulses
 *  instead — one element carrying both the semantic state (its text) and the
 *  liveness state (its beat), rather than two elements a reader has to
 *  correlate. The four-state computation below is unchanged and still the
 *  point: `done` (has a terminal record) and `animate` (plausibly executing)
 *  answer different questions, and a run that opened and went silent reads
 *  RUNNING while liveness says otherwise. That contradiction is the signal,
 *  so it survives as `data-live` on the pill.
 *
 *  The label rides `title` rather than `aria-label`: the pill already has an
 *  accessible name from its own text, and overriding it would trade one piece
 *  of information for another. `title` is exposed as a DESCRIPTION, and adds
 *  no text — which the parity golden requires, since it compares `#stage`
 *  innerText byte-for-byte. */
export type LivenessState = "beating" | "quiet" | "stale" | "done";

export function livenessState({
  done,
  animate,
  lastBeatMs,
  nowMs,
}: LivenessPulseProps & { nowMs: number }): {
  state: LivenessState;
  label: string;
} {
  // A PURE function, not a hook: the view already subscribes to the shared
  // clock and holds `nowMs`. Calling `useNowMs` again here added a second
  // hook behind the component's early returns — "Rendered more hooks than
  // during the previous render" — so the clock stays the caller's.
  const quiet = !animate || lastBeatMs == null || nowMs - lastBeatMs > PULSE_QUIET_AFTER_MS;
  const state: LivenessState = done ? "done" : !animate ? "stale" : quiet ? "quiet" : "beating";
  const label =
    state === "beating"
      ? "running"
      : state === "quiet"
        ? "running, no activity in the last few seconds"
        : state === "stale"
          ? "no recent activity, may be abandoned"
          : "finished";
  return { state, label };
}
