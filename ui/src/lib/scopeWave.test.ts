import { describe, it, expect } from "vitest";
import { LOBE_FADE_SEC, PHASE_STEP_CAP, SWEEP_PER_PHASE, lobeTarget, stepLobes, wavePhaseStep, type LobeBlend } from "./scopeWave";
import { waveAt } from "../components/TokenScope";
import { advanceMorph, createMorph } from "./scopeMorph";
import type { Rgb } from "./scopeTone";

/** The prototype's advance, which `wavePhaseStep` reproduces exactly below
 *  its knee: `(0.6 + 0.09·tps)` per normalized (60 Hz) frame. */
const prototype = (tps: number, dt: number) => (0.6 + tps * 0.09) * dt * 60;

describe("(#2911) wavePhaseStep: the GEN wave's visible speed follows the rate at any refresh rate", () => {
  for (const hz of [60, 120]) {
    it(`at ${hz} Hz the step rises strictly with the rate and never reaches Nyquist (half a lobe per frame)`, () => {
      const dt = 1 / hz;
      let prev = -1;
      for (let tps = 0; tps <= 200; tps += 1) {
        const step = wavePhaseStep(tps, dt, 1);
        expect(step, `tps ${tps}`).toBeGreaterThan(prev);
        expect(step, `tps ${tps}`).toBeLessThan(Math.PI);
        // The sweep dot orbits at half the phase; its Nyquist is half a
        // revolution per frame, so bounding the phase bounds it too.
        expect(step * SWEEP_PER_PHASE, `dot at tps ${tps}`).toBeLessThan(Math.PI);
        prev = step;
      }
    });
  }

  it("at 60 Hz, 50 tok/s no longer moves 0.81 of a lobe per frame (the issue's reverse rotation)", () => {
    expect(prototype(50, 1 / 60) / (2 * Math.PI)).toBeGreaterThan(0.8);
    expect(wavePhaseStep(50, 1 / 60, 1) / (2 * Math.PI)).toBeLessThan(0.5);
  });

  it("at 120 Hz the step is the prototype's, unchanged, up to 40 tok/s", () => {
    for (let tps = 0; tps <= 40; tps += 1) {
      expect(wavePhaseStep(tps, 1 / 120, 1)).toBeCloseTo(prototype(tps, 1 / 120), 12);
    }
  });

  it("a long frame (jank, a 0.1 s dt) stays under the bound at any rate", () => {
    expect(wavePhaseStep(300, 0.1, 1)).toBeLessThan(Math.PI);
  });

  it("a finished run's slow echo scales the same way, and a dt or tempo of 0 advances nothing", () => {
    expect(wavePhaseStep(30, 1 / 60, 0.3)).toBeCloseTo(prototype(30, 1 / 60) * 0.3, 12);
    expect(wavePhaseStep(30, 0, 1)).toBe(0);
    expect(wavePhaseStep(30, 1 / 60, 0)).toBe(0);
    expect(wavePhaseStep(Number.NaN, 1 / 60, 1)).toBe(0);
  });

  it("a non-finite input never yields NaN, which would poison the phase for good", () => {
    // An infinite rate or tempo is the most motion there is: the cap.
    expect(wavePhaseStep(Number.POSITIVE_INFINITY, 1 / 60, 1)).toBe(PHASE_STEP_CAP);
    expect(wavePhaseStep(30, 1 / 60, Number.POSITIVE_INFINITY)).toBe(PHASE_STEP_CAP);
    expect(wavePhaseStep(30, Number.POSITIVE_INFINITY, 1)).toBe(PHASE_STEP_CAP);
    // Anything without a direction is no motion.
    expect(wavePhaseStep(Number.NEGATIVE_INFINITY, 1 / 60, 1)).toBe(0);
    expect(wavePhaseStep(30, 1 / 60, Number.NaN)).toBe(0);
    expect(wavePhaseStep(30, Number.NaN, 1)).toBe(0);
    expect(wavePhaseStep(-50, 1 / 60, 1)).toBe(0);
    expect(wavePhaseStep(Number.POSITIVE_INFINITY, 0, 1)).toBe(0);
    // A finite rate large enough that the soft clip's own `u` overflows to
    // Infinity must still read as the cap, not `Infinity / Infinity`.
    expect(wavePhaseStep(Number.MAX_VALUE, 1 / 60, 10)).toBeCloseTo(PHASE_STEP_CAP, 12);
  });
});

describe("(#2911) the lobe crossfade never re-targets mid-fade", () => {
  const dt = 1 / 120;
  const rgb: Rgb = [0, 255, 0];

  it("replaying one heartbeat 10 -> 66 tok/s: no frame moves the trace by more than the crossfade's own step", () => {
    const m = createMorph();
    for (let i = 0; i < 600; i++) advanceMorph(m, "generating", 10, rgb, dt);
    const l: LobeBlend = { cur: 3, prev: 3, mix: 1 };
    let ease = stepLobes(l, 10, dt);
    const N = 240;
    // The phase is held so the frame-to-frame difference is the crossfade
    // alone, not the wave's own rotation.
    const profile = (e: number) => Array.from({ length: N }, (_, i) => waveAt((i / N) * Math.PI * 2, l.prev, l.cur, e, 0.7));
    let last = profile(ease);
    let worst = 0;
    const fired: Array<[number, number, number]> = [];
    for (let i = 0; i < 600; i++) {
      const p = advanceMorph(m, "generating", 66, rgb, dt);
      const before = { cur: l.cur, mix: l.mix };
      ease = stepLobes(l, p.wave, dt);
      if (l.cur !== before.cur) fired.push([before.cur, l.cur, before.mix]);
      const cur = profile(ease);
      for (let j = 0; j < N; j++) worst = Math.max(worst, Math.abs(cur[j] - last[j]));
      last = cur;
    }
    // The crossfade's largest per-frame step: the smoothstep's peak slope
    // (1.5) x the mix a frame advances x the largest difference between two
    // whole-lobe waves (two amplitudes, plus the harmonic's 12% on each).
    const normal = 1.5 * (dt / LOBE_FADE_SEC) * 2.24;
    expect(worst).toBeLessThanOrEqual(normal + 1e-9);
    expect(l.cur).toBe(6);
    // Every re-target waited for the previous fade to settle.
    for (const [, , mixAtFire] of fired) expect(mixAtFire).toBe(1);
    expect(fired.length).toBeGreaterThanOrEqual(2);
  });

  it("a second change while a fade runs waits for the fade to finish, then goes to the newest target", () => {
    const l: LobeBlend = { cur: 3, prev: 3, mix: 1 };
    stepLobes(l, 66, dt);
    expect(l).toMatchObject({ prev: 3, cur: 6 });
    expect(l.mix).toBeLessThan(1);
    stepLobes(l, 200, dt);
    expect(l).toMatchObject({ prev: 3, cur: 6 });
    let frames = 0;
    while (l.cur !== 8 && frames < 200) {
      stepLobes(l, 200, dt);
      frames += 1;
    }
    expect(l).toMatchObject({ prev: 6, cur: 8 });
    // It fired on the first frame after the 3 -> 6 fade settled: ~0.6 s.
    expect(frames).toBeGreaterThan(LOBE_FADE_SEC / dt - 3);
    expect(frames).toBeLessThan(LOBE_FADE_SEC / dt + 3);
  });

  it("a static frame (dt 0) settles the fade at once", () => {
    const l: LobeBlend = { cur: 3, prev: 3, mix: 1 };
    expect(stepLobes(l, 66, 0)).toBe(1);
    expect(l).toMatchObject({ prev: 3, cur: 6, mix: 1 });
  });

  it("the count moves by hysteresis, 0.6 of a lobe, from 3 to 8", () => {
    expect(lobeTarget(0)).toBe(3);
    expect(lobeTarget(22)).toBe(4);
    expect(lobeTarget(1000)).toBe(8);
    const l: LobeBlend = { cur: 4, prev: 4, mix: 1 };
    stepLobes(l, 22 + 0.5 * 22, dt); // 4.5: within 0.6 of 4, stays
    expect(l.cur).toBe(4);
    stepLobes(l, 22 + 0.7 * 22, dt); // 4.7: past it, moves to 5
    expect(l.cur).toBe(5);
  });
});
