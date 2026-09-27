import { describe, it, expect } from "vitest";
import {
  REST_CLOSED_FADE_MS,
  REST_HAND_TOP,
  REST_NUMBER_FLARE_MS,
  REST_STILL_FRAME,
  REST_STROKE_FLOOR,
  REST_TICK_GLOW_MS,
  pageClockRate,
  pageNowOf,
  restEffectMs,
  restHandAngle,
  restHandGeometry,
  restShownSeconds,
  restStrokeBrightness,
  restStrokeSegments,
  stepRestHand,
  type PageClock,
  type RestHandFrame,
  type RestHandState,
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

/** Run the hand over frames at the given wall times, reading `clock`. */
function run(clock: PageClock, walls: number[], seekGen = 0, start: RestHandState | null = null) {
  let state = start;
  const out: (RestHandFrame & { wallMs: number; state: RestHandState })[] = [];
  for (const wallMs of walls) {
    const r = stepRestHand(state, { end: END, pageMs: pageNowOf(clock, wallMs, 0), wallMs, rate: pageClockRate(clock), seekGen });
    state = r.state;
    out.push({ ...r.frame, wallMs, state: r.state });
  }
  return out;
}
const pb = (tMs: number, wallMs: number, rate: number): PageClock => ({ kind: "playback", tMs, wallMs, rate });
const frames60 = (from: number, to: number) => {
  const out: number[] = [];
  for (let t = from; t <= to; t += 1000 / 60) out.push(t);
  return out;
};

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
      const wallStart = 1234.567;
      const out = run(pb(END - 3300, wallStart, rate), frames60(wallStart, wallStart + 2000).filter((w) => (w - wallStart) * rate < 3300));
      let changes = 0;
      for (let i = 1; i < out.length; i++) {
        if (out[i].shown !== out[i - 1].shown) {
          changes += 1;
          expect(offTop(out[i].angle)).toBeLessThanOrEqual(frameSweep(rate) + 1e-9);
          expect(out[i].angle).toBeLessThan(out[i - 1].angle);
        } else {
          expect(out[i].angle).toBeGreaterThanOrEqual(out[i - 1].angle);
        }
      }
      expect(changes, `rate ${rate}: the number dropped`).toBeGreaterThan(0);
    }
  });

  it("reads the page clock: the wall clock live, the playhead from when the transport computed it, a frozen instant", () => {
    expect(pageNowOf({ kind: "wall" }, 123, 9_999)).toBe(9_999);
    expect(pageNowOf({ kind: "frozen", tMs: 42 }, 5_000, 9_999)).toBe(42);
    // At 5s/s, 100 wall ms after the transport's tick is 500 page ms.
    expect(pageNowOf(pb(10_000, 1_000, 5), 1_100, 0)).toBe(10_500);
    // Paused: the playhead, however much wall time passes.
    expect(pageNowOf(pb(10_000, 1_000, 0), 60_000, 0)).toBe(10_000);
    // A frame stamped before the transport's tick never reads before it.
    expect(pageNowOf(pb(10_000, 1_000, 5), 990, 0)).toBe(10_000);
    expect([pageClockRate({ kind: "wall" }), pageClockRate({ kind: "frozen", tMs: 0 }), pageClockRate(pb(0, 0, 30))]).toEqual([1, 0, 30]);
  });

  // (#2961 review, M1) The reviewer's probe: a clock READ at wall 1497 but
  // RENDERED at 1505 must extrapolate from 1497, and page time never moves
  // backward within one rest, so the countdown cannot climb back.
  it("a re-anchor behind the extrapolation never climbs the countdown back (M1 probe)", () => {
    // 5.5 s left at wall 0; the tick to "4s" at wall 1500.
    const before = run(pb(END - 5500, 0, 1), [0, 500, 1000, 1500, 1502]);
    expect(before.map((f) => f.shown)).toEqual([6, 5, 5, 4, 4]);
    // The page's next tick: the clock read at wall 1497 (END − 4003).
    const after = run(pb(END - 4003, 1497, 1), [1506, 1510, 1600], 0, before[before.length - 1].state);
    expect(after.map((f) => f.shown)).toEqual([4, 4, 4]);
    // No tick was seen again: no extra flare.
    expect(after.map((f) => f.state.flares)).toEqual([2, 2, 2]);
    // Even a clock that lands clearly behind (a late or skewed reading)
    // holds the hand where it was instead of rewinding it.
    const behind = run(pb(END - 4200, 1600, 1), [1601], 0, after[after.length - 1].state);
    expect(behind[0].shown).toBe(4);
    expect(behind[0].state.pageMs).toBeGreaterThanOrEqual(after[after.length - 1].state.pageMs);
  });

  it("a real seek (a new seek generation) may move page time backward, and starts with no tick seen", () => {
    const before = run(pb(END - 5500, 0, 1), [0, 500, 1000, 1500]);
    const seek = run(pb(END - 8950, 2000, 1), [2000, 2016], 1, before[before.length - 1].state);
    expect(seek.map((f) => f.shown)).toEqual([9, 9]);
    expect(seek[0].state.flares).toBe(0);
    expect(seek[0].glow).toBe(0);
    expect(seek[0].closedFade).toBe(0);
  });

  // (#2961 review, C1) Effects come from a tick the scope SAW.
  it("no glow, fading circle or flare from phase alone: first frame, play after a pause and a scrub, a resume just past a second", () => {
    // First frame 10 ms after a whole second.
    const first = run(pb(END - 3990, 0, 1), [0]);
    expect([first[0].glow, first[0].closedFade, first[0].state.flares]).toEqual([0, 0, 0]);
    // The reviewer's probe: observe a tick, pause, scrub to 3.95 s left, play
    // at 1x. The first frame after play must show neither.
    const seen = run(pb(END - 5500, 0, 1), [0, 500, 520]);
    expect(seen[2].closedFade).toBeGreaterThan(0.9);
    const paused = run(pb(END - 4980, 520, 0), [600, 3000], 0, seen[2].state);
    const scrubbed = run(pb(END - 3950, 3100, 0), [3100], 1, paused[1].state);
    const played = run(pb(END - 3950, 3200, 1), [3200, 3216], 1, scrubbed[0].state);
    expect(played[0].shown).toBe(4);
    expect([played[0].glow, played[0].closedFade, played[1].glow, played[1].closedFade]).toEqual([0, 0, 0, 0]);
    // A resume (no seek) just past a whole second: the number did not drop
    // between two frames the scope drew, so nothing fires.
    const paused2 = run(pb(END - 3990, 0, 0), [0, 5000]);
    const resumed = run(pb(END - 3990, 5000, 1), [5010], 0, paused2[1].state);
    expect([resumed[0].glow, resumed[0].closedFade, resumed[0].state.flares]).toEqual([0, 0, 0]);
  });

  it("the tick glow lasts ~180 ms and the finished circle ~220 ms of wall time from the observed tick", () => {
    const out = run(pb(END - 5500, 0, 1), [0, 400, 500, 590, 610, 680, 720]);
    const at = (w: number) => out.find((f) => f.wallMs === w)!;
    expect(REST_TICK_GLOW_MS).toBe(180);
    expect(REST_CLOSED_FADE_MS).toBe(220);
    expect(at(400).glow).toBe(0);
    expect(at(500).glow).toBe(1);
    expect(at(500).closedFade).toBe(1);
    expect(at(590).glow).toBeCloseTo(0.5, 9);
    expect(at(610).closedFade).toBeCloseTo(0.5, 9);
    expect(at(680).glow).toBe(0);
    expect(at(720).closedFade).toBeCloseTo(0, 9);
  });

  // (#2961 review, C2) At fast playback a page second is shorter than the
  // effects: each is cut to 80% of a tick's wall time so the ring clears.
  it("at fast playback the effects end before the next tick (rate 5 and 30)", () => {
    expect(restEffectMs(REST_CLOSED_FADE_MS, 1)).toBe(220);
    expect(restEffectMs(REST_CLOSED_FADE_MS, 0)).toBe(220);
    expect(restEffectMs(REST_CLOSED_FADE_MS, 5)).toBe(160);
    expect(restEffectMs(REST_TICK_GLOW_MS, 5)).toBe(160);
    expect(restEffectMs(REST_NUMBER_FLARE_MS, 5)).toBe(160);
    expect(restEffectMs(REST_CLOSED_FADE_MS, 30)).toBeCloseTo(26.667, 3);
    for (const rate of [5, 30]) {
      const period = 1000 / rate;
      // Frames every 2 ms, so a moment late in each tick is always sampled.
      const walls: number[] = [];
      for (let w = 0; w * rate < 9000; w += 2) walls.push(w);
      const out = run(pb(END - 9500, 0, rate), walls);
      expect(out[0].flareMs).toBeCloseTo(Math.min(260, 0.8 * period), 9);
      let checked = 0;
      for (const f of out) {
        if (f.state.tickWallMs !== null && f.wallMs - f.state.tickWallMs >= 0.85 * period) {
          checked += 1;
          expect(f.closedFade, `rate ${rate} at ${f.wallMs}`).toBe(0);
          expect(f.glow).toBe(0);
        }
      }
      expect(checked).toBeGreaterThan(0);
    }
  });

  // (#2961 review, C4) The final drop looks like every other tick.
  it("a rest ending: the drop to 0 is a tick (the last circle fades, the glow fires, it flares); nothing new is drawn", () => {
    const out = run(pb(END - 1500, 0, 1), [0, 500, 1400, 1500, 1600, 1800]);
    const end = out.find((f) => f.wallMs === 1500)!;
    expect(end).toMatchObject({ shown: 0, stroke: false, progress: 0, angle: REST_HAND_TOP });
    expect(end.closedFade).toBe(1);
    expect(end.glow).toBe(1);
    expect(end.state.flares).toBe(2);
    expect(out.find((f) => f.wallMs === 1800)!.closedFade).toBe(0);
    expect(restHandGeometry(END, END + 5000)).toEqual({ angle: REST_HAND_TOP, shown: 0, progress: 0, stroke: false });
  });

  it("reduced motion: the still frame is the dot at 12 with no stroke, glow or fade", () => {
    expect(REST_STILL_FRAME).toMatchObject({ angle: REST_HAND_TOP, progress: 0, stroke: false, glow: 0, closedFade: 0 });
  });

  // (#2961, design B) The dot draws the circle over each second.
  it("the drawn stroke runs from 12 o'clock to the dot: its length is the progress, 1 − frac(seconds left)", () => {
    for (const [left, progress] of [[7.75, 0.25], [7.5, 0.5], [7.1, 0.9], [7.999, 0.001]] as const) {
      const f = restHandGeometry(END, END - left * 1000);
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
      expect(segs[0].brightness).toBeCloseTo(1 - 0.75 * (progress - halfSeg), 12);
      expect(segs[segs.length - 1].brightness).toBeCloseTo(1 - 0.75 * halfSeg, 12);
      for (let i = 1; i < segs.length; i++) expect(segs[i].brightness).toBeGreaterThan(segs[i - 1].brightness);
    }
    const closed = restStrokeSegments(1);
    expect(closed[0].brightness).toBeCloseTo(0.25, 1);
    expect(closed[closed.length - 1].brightness).toBeCloseTo(1, 1);
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
