import { describe, it, expect } from "vitest";
import { tubeSize, stackedTubeSize, TUBE_MIN, TUBE_MAX, STACKED_MIN, STACKED_MAX } from "./tubeFit";

// (#2890) The arithmetic behind a card's tube; the DOM measurement around it
// needs real layout and is checked in rendered screenshots.
describe("tubeSize (phone: beside the text)", () => {
  it("stops where the text column's floor begins", () => {
    // A 326px phone card: 326 - 184 - 12 = 130, under its 45% share (146).
    expect(tubeSize(326)).toBe(130);
  });
  it("a wide card stops at its width share, then the cap", () => {
    expect(tubeSize(360)).toBe(150);
  });
  it("never goes below the minimum or above the maximum", () => {
    expect(tubeSize(200)).toBe(TUBE_MIN);
    expect(tubeSize(900)).toBe(TUBE_MAX);
  });
});

describe("stackedTubeSize (desktop: between header and status)", () => {
  it("is 55% of the card's inner width", () => {
    // A 332px desktop card has 300px inside: 165px.
    expect(stackedTubeSize(300)).toBe(165);
  });
  it("is clamped both ways", () => {
    expect(stackedTubeSize(150)).toBe(STACKED_MIN);
    expect(stackedTubeSize(428)).toBe(STACKED_MAX);
  });
});
