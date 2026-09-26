/**
 * (#2911) The GEN wave's two free-running rules, pulled out of
 * `TokenScope.tsx`'s `drawFrame` so they can be pinned by tests: how far the
 * phase advances per frame, and when the whole-lobe count crossfades.
 *
 * Pure scalar math, no allocation: both run once per animation frame.
 */

/** The prototype's phase speed, in radians per NORMALIZED (60 Hz) frame:
 *  `0.6 + 0.09·tps`. Approved on 120 Hz hardware, where a frame is half of
 *  that. */
const PHASE_BASE = 0.6;
const PHASE_PER_TPS = 0.09;

/** Up to this many radians per frame the step is the prototype's exactly
 *  (0.35 of a lobe: at 120 Hz everything up to 40 tok/s is unchanged).
 *  Above it the step is compressed smoothly toward `PHASE_STEP_CAP`. */
export const PHASE_STEP_KNEE = 0.35 * Math.PI * 2;
/** The most the wave may move per frame, 0.45 of a lobe, under Nyquist (half
 *  a lobe, `π` in phase). A `k`-lobe pattern that jumps more than half a lobe
 *  between two frames reads as moving BACKWARD (the issue's measurement: on
 *  60 Hz at 50 tok/s the prototype moved 0.81 of a lobe per frame, a slow
 *  reverse rotation; near 70 tok/s it looked still). Below the bound a
 *  faster rate always looks faster, at any refresh rate. */
export const PHASE_STEP_CAP = 0.45 * Math.PI * 2;

/** The sweep dot's angle is `-phase · SWEEP_PER_PHASE`, so its per-frame
 *  motion is half the wave's; bounding the phase bounds the dot under its
 *  own Nyquist (half a revolution) with room to spare. */
export const SWEEP_PER_PHASE = 0.5;

/** How far the wave's phase advances this frame, for a rate `tps`, a frame
 *  of `dt` seconds and the morph's `tempo` (1 live; slow for a finished
 *  run's echo). Frame-rate aware: the SAME rate on a 60 Hz display asks for
 *  twice the step of a 120 Hz one, and the compression above the knee is
 *  what keeps that step short of half a lobe. Strictly increasing in the
 *  rate, so "speed follows the rate" holds on either display.
 *
 *  Note the harmonic term in `TokenScope`'s `waveAt` runs at 1.7x the phase,
 *  so it crosses its own Nyquist earlier (a step of `π / 1.7`); this bound
 *  is for the base wave, which carries the motion the eye follows. */
export function wavePhaseStep(tps: number, dt: number, tempo: number): number {
  const raw = (PHASE_BASE + PHASE_PER_TPS * tps) * dt * 60 * tempo;
  if (!(raw > 0)) return 0;
  if (raw <= PHASE_STEP_KNEE) return raw;
  // A rational soft clip, `u / (1 + u)`: slope 1 at the knee (no kink) and
  // strictly increasing all the way, where `tanh` would saturate exactly in
  // floating point past a few hundred tok/s at 60 Hz.
  const room = PHASE_STEP_CAP - PHASE_STEP_KNEE;
  const u = (raw - PHASE_STEP_KNEE) / room;
  return PHASE_STEP_KNEE + room * (u / (1 + u));
}

/** The wave's WHOLE lobe count now, the one it is leaving, and how far the
 *  crossfade between them has run (0..1, 1 = settled). */
export interface LobeBlend {
  cur: number;
  prev: number;
  mix: number;
}

export const LOBE_MIN = 3;
export const LOBE_MAX = 8;
const LOBE_PER_TPS = 1 / 22;
/** The count only moves when the rate's own (fractional) count is more than
 *  this far from the drawn one, so a rate hovering at a boundary does not
 *  flicker between two counts. */
export const LOBE_HYSTERESIS = 0.6;
/** How long a crossfade between two counts takes, in seconds. */
export const LOBE_FADE_SEC = 0.6;

/** The lobe count the rate maps to, rounded to a whole number. */
export function lobeTarget(tps: number): number {
  return Math.round(Math.min(LOBE_MAX, LOBE_MIN + tps * LOBE_PER_TPS));
}

/** Advance the crossfade one frame and return its eased mix. A change of
 *  target is taken up ONLY once the running fade has settled (`mix` at 1):
 *  re-targeting mid-fade used to discard the blend in progress and start a
 *  new one from the count it had not yet reached, a radial jump of up to a
 *  full amplitude (the issue: 4 -> 5 firing at mix 0.22, then 5 -> 6 at
 *  0.53, on one heartbeat going 10 -> 66 tok/s). Deferring it, the fade
 *  finishes, then the next one starts from a settled trace toward whatever
 *  the rate says NOW, so the trace never pops. A static frame (dt 0,
 *  reduced motion) settles at once. */
export function stepLobes(l: LobeBlend, tps: number, dt: number): number {
  if (l.mix >= 1) {
    const exact = Math.min(LOBE_MAX, LOBE_MIN + tps * LOBE_PER_TPS);
    if (Math.abs(exact - l.cur) > LOBE_HYSTERESIS) {
      l.prev = l.cur;
      l.cur = Math.round(exact);
      l.mix = 0;
    }
  }
  l.mix = dt > 0 ? Math.min(1, l.mix + dt / LOBE_FADE_SEC) : 1;
  return l.mix * l.mix * (3 - 2 * l.mix);
}
