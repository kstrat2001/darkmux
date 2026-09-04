/**
 * (#2325) The mission graph went blank a second after painting because React
 * Flow's controlled `nodes` path drops its own measurements — see
 * `measuredDims.ts`'s doc. These cover the feedback path that carries them.
 */
import { describe, expect, it } from "vitest";
import type { Node, NodeChange } from "reactflow";
import { recordDimensions, withMeasuredDimensions } from "./measuredDims";

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
