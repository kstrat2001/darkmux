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
import { useNowMs } from "../lib/clock";

/** How long without proof of life before the pulse holds still. 2.5x the
 *  emitter's `HEARTBEAT_MIN_INTERVAL` (2s). */
export const PULSE_QUIET_AFTER_MS = 5000;

export interface LivenessPulseProps {
  /** Whether the run is still going at all. A finished run never pulses. */
  live: boolean;
  /** When the newest record for this run landed, or `null` if none has. */
  lastBeatMs: number | null;
}

export function LivenessPulse({ live, lastBeatMs }: LivenessPulseProps) {
  // Subscribed only while live — the shared clock's timer is gated on having
  // an active subscriber, so a finished run drives nothing.
  const nowMs = useNowMs(live);
  const quiet = !live || lastBeatMs == null || nowMs - lastBeatMs > PULSE_QUIET_AFTER_MS;
  const state = !live ? "done" : quiet ? "quiet" : "beating";

  return (
    <span
      className="pulse"
      data-state={state}
      // The state is announced, not just drawn: `beating` vs `quiet` is a real
      // status distinction and colour/motion alone would not carry it.
      role="status"
      aria-label={state === "beating" ? "running" : state === "quiet" ? "running, no recent activity" : "finished"}
      title={state === "beating" ? "running" : state === "quiet" ? "no activity in the last few seconds" : "finished"}
    />
  );
}
