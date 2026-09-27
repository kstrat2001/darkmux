import { describe, expect, it } from "vitest";
import { leftTrimWidth } from "./leftTrim";

// (#2963) A path trimmed from the left draws its ellipsis right before the
// first WHOLE character that fits, so whatever part of a character does not
// fit opened a gap between the "·" before the box and the "…". The box is
// narrowed to the ellipsis plus a whole number of characters, so the "…"
// sits at the box's left edge, where an untrimmed path's first character is.
describe("leftTrimWidth (#2963)", () => {
  it("narrows an overflowing box to the ellipsis plus whole characters", () => {
    // 20 chars at 10px (200px) in a 95px box, 10px ellipsis: 8 whole chars.
    expect(leftTrimWidth({ available: 95, full: 200, chars: 20, ellipsis: 10 })).toBe(90.5);
  });

  it("leaves a box that holds the whole path alone", () => {
    expect(leftTrimWidth({ available: 200, full: 200, chars: 20, ellipsis: 10 })).toBeNull();
    expect(leftTrimWidth({ available: 250, full: 200, chars: 20, ellipsis: 10 })).toBeNull();
  });

  it("never narrows below the ellipsis, and does nothing it cannot measure", () => {
    expect(leftTrimWidth({ available: 12, full: 200, chars: 20, ellipsis: 10 })).toBe(10.5);
    expect(leftTrimWidth({ available: 0, full: 0, chars: 20, ellipsis: 10 })).toBeNull();
    expect(leftTrimWidth({ available: 95, full: 200, chars: 0, ellipsis: 10 })).toBeNull();
  });
});
