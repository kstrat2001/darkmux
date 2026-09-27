import { describe, it, expect } from "vitest";
import { playbackClockOf } from "./pageClockRate";

// (#2961 review, C3a) The value App provides: a paused transport's clock
// stands still (rate 0) even though its speed setting keeps its value, and
// there is no clock when the lenses are not on a playhead.
describe("playbackClockOf", () => {
  const transport = { t: 5_000, tickWallMs: 123, playing: true, speed: 30 };
  it("while playing: the playhead, when the transport computed it, and the speed", () => {
    expect(playbackClockOf(transport, 5_000)).toEqual({ kind: "playback", tMs: 5_000, wallMs: 123, rate: 30 });
  });
  it("paused: rate 0, so the hand stops", () => {
    expect(playbackClockOf({ ...transport, playing: false }, 5_000)).toEqual({ kind: "playback", tMs: 5_000, wallMs: 123, rate: 0 });
  });
  it("no playhead: no clock", () => {
    expect(playbackClockOf(transport, null)).toBeNull();
  });
});
