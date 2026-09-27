import { describe, it, expect } from "vitest";
import {
  REST_HAND_TOP,
  REST_TICK_GLOW_MS,
  pageNowAt,
  restHandAngle,
  restHandFrame,
  restShownSeconds,
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

  it("a rest ending: the number reaches 0, the hand rests at the top, no trail or glow", () => {
    for (const now of [END, END + 1, END + 5000]) {
      expect(restHandFrame(END, now, 1, false)).toEqual({ angle: REST_HAND_TOP, shown: 0, trail: false, glow: 0 });
    }
    // The last second still runs normally.
    expect(restHandFrame(END, END - 400, 1, false)).toMatchObject({ shown: 1, trail: true });
  });

  it("reduced motion: one still frame, the hand at the top, no trail or glow; the number still counts", () => {
    for (const now of [END - 4000, END - 3700, END - 3001]) {
      const f = restHandFrame(END, now, 1, true);
      expect(f).toEqual({ angle: REST_HAND_TOP, shown: restShownSeconds(END, now), trail: false, glow: 0 });
    }
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
