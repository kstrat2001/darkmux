import { describe, it, expect } from "vitest";
import { tubeSize, TUBE_MIN, TUBE_MAX } from "./tubeFit";

// (#2890) The arithmetic behind a card's tube square; the DOM measurement
// around it needs real layout and is checked in rendered screenshots.
describe("tubeSize", () => {
  it("fills the row's height when the width allows it", () => {
    // A phone card (326px inner) leaves 130px after the 184px text floor.
    expect(tubeSize(326, 110)).toBe(110);
  });
  it("stops where the text column's floor begins", () => {
    // A 300px desktop card: 300 - 184 - 12 = 104, under its 45% share.
    expect(tubeSize(300, 147)).toBe(104);
  });
  it("a card alone on its row is bounded by width only", () => {
    expect(tubeSize(326, null)).toBe(130);
  });
  it("a wide card stops at its width share", () => {
    // 400px: 45% is 180, the floor leaves 204, so 180 (then the 150 cap).
    expect(tubeSize(400, null)).toBe(150);
    // 360px: 45% is 162, the floor leaves 164; the 150 cap wins.
    expect(tubeSize(360, null)).toBe(150);
  });
  it("never goes below the minimum or above the maximum", () => {
    expect(tubeSize(300, 60)).toBe(TUBE_MIN);
    expect(tubeSize(900, null)).toBe(TUBE_MAX);
  });
});
