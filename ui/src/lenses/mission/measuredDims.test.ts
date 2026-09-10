/**
 * (#2325) The mission graph went blank a second after painting because React
 * Flow's controlled `nodes` path drops its own measurements — see
 * `measuredDims.ts`'s doc. These cover the feedback path that carries them.
 */
import { describe, expect, it } from "vitest";
import type { Node, NodeChange } from "reactflow";
import { clampCanvasHeight, recordDimensions, withMeasuredDimensions } from "./measuredDims";

const dimChange = (id: string, width: number, height: number): NodeChange => ({
  id,
  type: "dimensions",
  dimensions: { width, height },
});

const node = (id: string, extra: Partial<Node> = {}): Node => ({
  id,
  position: { x: 0, y: 0 },
  data: {},
  ...extra,
});

describe("recordDimensions", () => {
  it("records a measurement per node id", () => {
    const dims = recordDimensions({}, [dimChange("a", 496, 120), dimChange("b", 320, 88)]);
    expect(dims).toEqual({ a: { width: 496, height: 120 }, b: { width: 320, height: 88 } });
  });

  it("ignores changes that are not dimensions", () => {
    const before = { a: { width: 496, height: 120 } };
    const after = recordDimensions(before, [
      { id: "a", type: "select", selected: true },
      { id: "a", type: "position", position: { x: 9, y: 9 } },
    ]);
    expect(after).toBe(before);
  });

  it("ignores a zero measurement — an unlaid-out node must not be recorded as measured", () => {
    expect(recordDimensions({}, [dimChange("a", 0, 0), dimChange("b", 496, 0)])).toEqual({});
  });

  it("returns the SAME map when every measurement is unchanged", () => {
    const before = { a: { width: 496, height: 120 } };
    expect(recordDimensions(before, [dimChange("a", 496, 120)])).toBe(before);
  });

  it("takes a new measurement when a card's own size changed", () => {
    const before = { a: { width: 496, height: 120 } };
    const after = recordDimensions(before, [dimChange("a", 496, 164)]);
    expect(after).not.toBe(before);
    expect(after.a).toEqual({ width: 496, height: 164 });
  });
});

describe("withMeasuredDimensions", () => {
  it("stamps the measurement onto a freshly rebuilt node", () => {
    // The regression itself: a rebuilt node carries no width/height, so React
    // Flow's node wrapper renders it `visibility: hidden`.
    const rebuilt = [node("a", { style: { width: 496 } })];
    const [stamped] = withMeasuredDimensions(rebuilt, { a: { width: 496, height: 120 } });
    expect(stamped.width).toBe(496);
    expect(stamped.height).toBe(120);
    // The layout's own style width is untouched — it is a different decision.
    expect(stamped.style).toEqual({ width: 496 });
  });

  it("leaves a node React Flow has not measured yet alone", () => {
    const unmeasured = node("new");
    const [out] = withMeasuredDimensions([unmeasured], { a: { width: 496, height: 120 } });
    expect(out).toBe(unmeasured);
    expect(out.width).toBeUndefined();
  });

  it("keeps node object identity when the measurement already matches", () => {
    const already = node("a", { width: 496, height: 120 });
    const [out] = withMeasuredDimensions([already], { a: { width: 496, height: 120 } });
    expect(out).toBe(already);
  });

  it("round-trips: what recordDimensions saw is what the next rebuild carries", () => {
    const dims = recordDimensions({}, [dimChange("a", 496, 120), dimChange("b", 320, 88)]);
    const out = withMeasuredDimensions([node("a"), node("b"), node("c")], dims);
    expect(out.map((n) => [n.id, n.width, n.height])).toEqual([
      ["a", 496, 120],
      ["b", 320, 88],
      ["c", undefined, undefined],
    ]);
  });
});

describe("clampCanvasHeight", () => {
  it("with no floor supplied, uses the real available space even when it is small — the #2520 round-1 shape", () => {
    // #2520's own measurement: an 844×390 landscape phone leaves ~103px
    // above the fixed phone drawer. Round 1 dropped the old flat `240`
    // floor entirely (no `floor` argument at all) so this returns
    // `available` untouched — which is exactly what made the #2618
    // regression possible: nothing here shrinks a too-small pane, but
    // nothing keeps it legible either. That is now the CALLER's job (see
    // `MissionCanvas.tsx`'s own doc on where `floor` comes from).
    expect(clampCanvasHeight(103.3125)).toBeCloseTo(103.3125);
  });

  it("still returns a valid positive length when available space is exhausted", () => {
    expect(clampCanvasHeight(0)).toBeGreaterThan(0);
    expect(clampCanvasHeight(-40)).toBeGreaterThan(0);
  });

  it("passes through generous desktop/portrait-phone room untouched", () => {
    expect(clampCanvasHeight(560)).toBe(560);
  });

  it("(#2618) prefers a caller-supplied floor over a smaller available space", () => {
    // The landscape-phone case this function's second parameter exists for:
    // ~103px available, but the container already reserves 480px via CSS
    // (`.missionlens .body`'s `min-height`) — the canvas should fill that
    // reserved room rather than shrink to the sliver visible without
    // scrolling.
    expect(clampCanvasHeight(103.3125, 480)).toBe(480);
  });

  it("(#2618) never shrinks below available space just because the floor is smaller", () => {
    // Portrait/desktop: plenty of room, well above any legible-content
    // floor — the floor must be a MINIMUM, not a ceiling.
    expect(clampCanvasHeight(560, 480)).toBe(560);
  });

  it("(#2618) still enforces the tiny MIN_VALID_CANVAS_PX safety floor even below a zero floor", () => {
    expect(clampCanvasHeight(-40, 0)).toBeGreaterThan(0);
  });
});
