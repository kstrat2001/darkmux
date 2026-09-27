import { describe, it, expect } from "vitest";
import {
  REST_CLOSED_FADE_MS,
  REST_HAND_TOP,
  REST_STROKE_FLOOR,
  REST_TICK_GLOW_MS,
  pageNowAt,
  restHandAngle,
  restHandFrame,
  restShownSeconds,
  restStrokeBrightness,
  restStrokeSegments,
} from "./restHand";
import { deriveLiveState } from "./tokenRate";
import type { FlowRecord } from "../types/handwritten";

const TURN = 2 * Math.PI;
/** How far the hand sweeps in one 60 Hz frame at page rate `rate`. */
const frameSweep = (rate: number) => TURN * (1000 / 60 / 1000) * rate;
/** Angular distance to the top, either way round. */
const offTop = (a: number) => {
  const d = (((a - REST_HAND_TOP) % TURN) + TURN) % TURN;
  return Math.min(d, TURN - d);
};

const END = Date.parse("2026-09-27T12:00:28Z");

// (#2961) The seconds hand's sync with the countdown.
describe("restHand", () => {
  it("at a tick (a whole number of seconds left) the hand is exactly at the top and the number has just dropped", () => {
    for (const left of [1, 2, 5, 12, 20]) {
      const now = END - left * 1000;
      expect(restHandAngle(END, now)).toBe(REST_HAND_TOP);
      expect(restShownSeconds(END, now)).toBe(left);
      // One ms earlier the number is still the higher one, and the hand is
      // about to reach the top from the left (nearly a full turn round).
      expect(restShownSeconds(END, now - 1)).toBe(left + 1);
      expect(restHandAngle(END, now - 1)).toBeCloseTo(REST_HAND_TOP + TURN * 0.999, 9);
    }
  });

  it("the hand makes one clockwise loop per second: a quarter turn a quarter second after a tick", () => {
    const tick = END - 7000;
    expect(restHandAngle(END, tick + 250)).toBeCloseTo(REST_HAND_TOP + TURN * 0.25, 9);
    expect(restHandAngle(END, tick + 500)).toBeCloseTo(REST_HAND_TOP + TURN * 0.5, 9);
    expect(restHandAngle(END, tick + 750)).toBeCloseTo(REST_HAND_TOP + TURN * 0.75, 9);
  });

  it("stepping 60 Hz frames through a tick, the frame the number changes on is the frame the hand crosses the top", () => {
    for (const rate of [1, 5, 30]) {
      // Start at an offset that does not land a frame exactly on the tick.
      const wallStart = 1234.567;
      const anchor = { pageMs: END - 3300, wallMs: wallStart, rate };
      let prevShown: number | null = null;
      let prevAngle: number | null = null;
      let changes = 0;
      for (let f = 0; f < 60 * 2; f++) {
        const now = pageNowAt(anchor, wallStart + f * (1000 / 60));
        if (now >= END) break;
        const { angle, shown } = restHandFrame(END, now, rate, false);
        if (prevShown !== null && shown !== prevShown) {
          changes += 1;
          // The hand is within one frame's sweep past the top ...
          expect(offTop(angle)).toBeLessThanOrEqual(frameSweep(rate) + 1e-9);
          // ... and the frame before it had not yet reached it (it wrapped).
          expect(angle).toBeLessThan(prevAngle!);
        } else if (prevAngle !== null) {
          // Between ticks the hand only moves forward.
          expect(angle).toBeGreaterThanOrEqual(prevAngle);
        }
        prevShown = shown;
        prevAngle = angle;
      }
      expect(changes, `rate ${rate}: the number dropped`).toBeGreaterThan(0);
    }
  });

  it("follows the page clock's rate: at 5s/s one wall second is five loops and five ticks", () => {
    const anchor = { pageMs: END - 10_000, wallMs: 0, rate: 5 };
    expect(restShownSeconds(END, pageNowAt(anchor, 0))).toBe(10);
    expect(restShownSeconds(END, pageNowAt(anchor, 1000))).toBe(5);
    // 100 wall ms at 5s/s is half a page second: half a turn.
    expect(restHandAngle(END, pageNowAt(anchor, 100))).toBeCloseTo(REST_HAND_TOP + TURN * 0.5, 9);
    // Paused (rate 0): the page clock, and so the hand, stands still.
    const paused = { ...anchor, rate: 0 };
    expect(pageNowAt(paused, 5000)).toBe(anchor.pageMs);
    expect(restHandFrame(END, pageNowAt(paused, 5000), 0, false).glow).toBe(0);
    // Never runs backward from its anchor.
    expect(pageNowAt(anchor, -500)).toBe(anchor.pageMs);
  });

  it("the tick glow lasts ~180 ms of wall time, whatever the playback rate", () => {
    const tick = END - 4000;
    expect(restHandFrame(END, tick, 1, false).glow).toBe(1);
    expect(restHandFrame(END, tick + 90, 1, false).glow).toBeCloseTo(0.5, 9);
    expect(restHandFrame(END, tick + REST_TICK_GLOW_MS, 1, false).glow).toBe(0);
    expect(restHandFrame(END, tick + 500, 1, false).glow).toBe(0);
    // At 5s/s, 90 wall ms is 450 page ms.
    expect(restHandFrame(END, tick + 450, 5, false).glow).toBeCloseTo(0.5, 9);
  });

  it("a rest ending: the number reaches 0, the dot rests at the top, nothing drawn, no glow or fade", () => {
    for (const now of [END, END + 1, END + 5000]) {
      expect(restHandFrame(END, now, 1, false)).toEqual({ angle: REST_HAND_TOP, shown: 0, progress: 0, stroke: false, glow: 0, closedFade: 0 });
    }
    // The last second still runs normally.
    expect(restHandFrame(END, END - 400, 1, false)).toMatchObject({ shown: 1, stroke: true, progress: 0.6 });
  });

  it("reduced motion: one still frame, the dot at 12 with no stroke, glow or fade; the number still counts", () => {
    for (const now of [END - 4000, END - 3700, END - 3001]) {
      const f = restHandFrame(END, now, 1, true);
      expect(f).toEqual({ angle: REST_HAND_TOP, shown: restShownSeconds(END, now), progress: 0, stroke: false, glow: 0, closedFade: 0 });
    }
  });

  // (#2961, design B) The dot draws the circle over each second.
  it("the drawn stroke runs from 12 o'clock to the dot: its length is the progress, 1 − frac(seconds left)", () => {
    for (const [left, progress] of [[7.75, 0.25], [7.5, 0.5], [7.1, 0.9], [7.999, 0.001]] as const) {
      const f = restHandFrame(END, END - left * 1000, 1, false);
      expect(f.progress).toBeCloseTo(progress, 9);
      expect(f.stroke).toBe(true);
      const segs = restStrokeSegments(f.progress);
      expect(segs[0].from).toBe(0);
      expect(segs[segs.length - 1].to).toBeCloseTo(f.progress, 12);
      for (let i = 1; i < segs.length; i++) expect(segs[i].from).toBeCloseTo(segs[i - 1].to, 12);
    }
    expect(restStrokeSegments(0)).toEqual([]);
  });

  it("the stroke fades by age: 1.0 at the dot, 1 − 0.75·progress at 12, 0.25 when the circle closes", () => {
    expect(REST_STROKE_FLOOR).toBe(0.25);
    expect(restStrokeBrightness(0)).toBe(1);
    expect(restStrokeBrightness(1)).toBe(0.25);
    expect(restStrokeBrightness(0.4)).toBeCloseTo(0.7, 12);
    for (const progress of [0.3, 0.6, 1]) {
      const segs = restStrokeSegments(progress);
      const halfSeg = progress / segs.length / 2;
      // Oldest (at 12) and newest (at the dot), each at its midpoint's age.
      expect(segs[0].brightness).toBeCloseTo(1 - 0.75 * (progress - halfSeg), 12);
      expect(segs[segs.length - 1].brightness).toBeCloseTo(1 - 0.75 * halfSeg, 12);
      // Brighter toward the dot, all along.
      for (let i = 1; i < segs.length; i++) expect(segs[i].brightness).toBeGreaterThan(segs[i - 1].brightness);
    }
    // A closed circle starts at ~25% and ends at ~100%.
    const closed = restStrokeSegments(1);
    expect(closed[0].brightness).toBeCloseTo(0.25, 1);
    expect(closed[closed.length - 1].brightness).toBeCloseTo(1, 1);
  });

  it("at the tick the finished circle fades out over ~220 ms of wall time while the next one starts", () => {
    const tick = END - 6000;
    expect(REST_CLOSED_FADE_MS).toBe(220);
    expect(restHandFrame(END, tick, 1, false)).toMatchObject({ closedFade: 1, progress: 0 });
    expect(restHandFrame(END, tick + 110, 1, false).closedFade).toBeCloseTo(0.5, 9);
    expect(restHandFrame(END, tick + 220, 1, false).closedFade).toBeCloseTo(0, 9);
    expect(restHandFrame(END, tick + 700, 1, false).closedFade).toBe(0);
    // At 5s/s, 110 wall ms is 550 page ms.
    expect(restHandFrame(END, tick + 550, 5, false).closedFade).toBeCloseTo(0.5, 9);
    // Paused: nothing fades, so there is no fading circle.
    expect(restHandFrame(END, tick + 10, 0, false).closedFade).toBe(0);
  });

  it("agrees with the countdown the page derives: deriveLiveState's seconds left and end time", () => {
    const d = "2026-09-27";
    const recs: FlowRecord[] = [
      { ts: `${d}T12:00:00Z`, action: "dispatch.start", session_id: "s1", source: "dispatch" } as unknown as FlowRecord,
      { ts: `${d}T12:00:08Z`, action: "dispatch.rest", session_id: "s1", payload: { ms: 20000 } } as unknown as FlowRecord,
    ];
    for (const offset of [0, 1, 999, 1000, 1001, 7250, 19_999]) {
      const now = Date.parse(`${d}T12:00:08Z`) + offset;
      const r = deriveLiveState(recs, now);
      expect(r.state).toBe("rest");
      expect(r.restEndMs).toBe(Date.parse(`${d}T12:00:28Z`));
      expect(restShownSeconds(r.restEndMs!, now)).toBe(r.restSecondsLeft);
    }
  });
});
